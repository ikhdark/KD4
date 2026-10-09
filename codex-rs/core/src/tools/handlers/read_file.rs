use std::collections::BTreeMap;

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
use crate::tools::command_output_artifact::create_canonical_output_artifact;
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

#[path = "code_sections.rs"]
pub(super) mod code_sections;

#[path = "read_file_structure.rs"]
mod structure;
use structure::FileSelector;

pub(crate) struct ReadFileHandler;
pub(crate) struct ReadStatusHandler;

impl ToolExecutor<ToolInvocation> for ReadStatusHandler {
    fn tool_name(&self) -> ToolName { ToolName::plain("read_status") }

    fn spec(&self) -> ToolSpec {
        let mut paths = JsonSchema::array(JsonSchema::string(None), Some("1–32 file paths; no source files are read.".into()));
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
                    ("snapshot_offset".into(), JsonSchema::integer(Some("Continue at next_snapshot_offset; default 0.".into()))),
                    ("range_offset".into(), JsonSchema::integer(Some("Continue at next_range_offset for an exact snapshot; default 0.".into()))),
                    ("artifact_offset".into(), JsonSchema::integer(Some("Continue at next_artifact_offset for an exact snapshot; default 0.".into()))),
                ]), None, Some(false.into()))),
            ]), Some(vec!["paths".into()]), Some(false.into())),
            output_schema: Some(json!({"type":"object", "properties":{
                "paths":{"type":"array", "items":{"type":"object"}}, "scope":{"type":"string"}},
                "required":["paths","scope"], "additionalProperties":false}).into()),
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool { true }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                paths: Vec<String>, environment_id: Option<String>,
                #[serde(default)]
                query: crate::tool_history::ReadStatusQuery,
            }
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel("read_status requires function arguments".into()));
            };
            let args: Args = parse_arguments(arguments)?;
            if args.paths.is_empty() || args.paths.len() > 32 || args.paths.iter().any(|path| path.trim().is_empty()) {
                return Err(FunctionCallError::RespondToModel("read_status requires 1–32 nonempty paths".into()));
            }
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            ).await?.ok_or_else(|| FunctionCallError::RespondToModel(
                "read_status requires a selected execution environment.".into()
            ))?;
            let paths = args.paths.iter().map(|path| environment.cwd().join(path)
                .map(|path| std::path::PathBuf::from(path.inferred_native_path_string()))
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string())))
                .collect::<Result<Vec<_>, _>>()?;
            let history = invocation.session.clone_history().await;
            let result = history.read_status_page(&paths, Some(&environment.environment_id), &args.query);
            Ok(boxed_tool_output(JsonToolOutput::new(result)))
        })
    }
}

impl CoreToolRuntime for ReadStatusHandler {}

struct PendingSourceInspection {
    store: std::sync::Arc<codex_agent_task_store::LocalAgentTaskStore>,
    start: codex_agent_task_store::SourceInspectionStart,
}

#[derive(Deserialize)]
#[serde(try_from = "RawReadFileArgs")]
struct ReadFileArgs {
    path: String,
    environment_id: Option<String>,
    selectors: Option<Vec<FileSelector>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReadFileArgs {
    #[serde(alias = "file_path")]
    path: String,
    environment_id: Option<String>,
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
            let end = start.checked_add(limit - 1)
                .ok_or("line range is too large")?;
            Some(vec![FileSelector::Exact(ToolOutputSelector::Lines { start, end })])
        } else {
            raw.selectors
        };
        Ok(Self { path: raw.path, environment_id: raw.environment_id, selectors })
    }
}

pub(crate) fn validate_read_file_arguments(arguments: &str) -> Result<(), String> {
    parse_arguments::<ReadFileArgs>(arguments).map(|_| ()).map_err(|error| error.to_string())
}

/// Use the handler's parser for replay identity, not a second interpretation of
/// legacy aliases/defaults. Path spelling and selector order remain exact;
/// filesystem identity and authorization still belong to the replay guard.
pub(crate) fn canonical_read_file_arguments(arguments: &str) -> Option<serde_json::Value> {
    let args = parse_arguments::<ReadFileArgs>(arguments).ok()?;
    // A live turn snapshot is never replayable under a filesystem identity.
    if args.path == crate::turn_diff_tracker::TURN_DIFF_LOCATOR { return None; }
    Some(json!({
        "path": args.path,
        "environment_id": args.environment_id,
        "selectors": args.selectors,
    }))
}

/// Reuse only fully delivered, self-contained selections from the same request
/// scope. This is a projection, not freshness proof: dispatch must still verify
/// the original authorization, environment and source observations.
pub(crate) fn reselect_read_file_output(
    previous: &str,
    requested: &str,
    output: &serde_json::Value,
) -> Option<serde_json::Value> {
    let previous = canonical_read_file_arguments(previous)?;
    let requested = canonical_read_file_arguments(requested)?;
    if previous["path"] != requested["path"]
        || previous["environment_id"] != requested["environment_id"]
    {
        return None;
    }
    let selectors = requested["selectors"].as_array()?;
    if selectors.is_empty() || selectors.len() > READ_TOOL_OUTPUT_MAX_SELECTORS {
        return None;
    }
    let retained = output["results"].as_array()?;
    // Preserve the cheap exact-range path before rebuilding a full snapshot.
    // Search hydration is not self-contained and must use the selector engine.
    let ranges = selectors.iter().map(|selector| {
        if !matches!(selector["kind"].as_str(), Some("bytes" | "lines")) { return None; }
        retained.iter().find_map(|result| retained_read_selection(result, selector))
    }).collect::<Option<Vec<_>>>();
    let selected = if let Some(ranges) = ranges {
        json!(ranges)
    } else if output["file_complete"] == true {
        // Complete explicit reads carry the same authenticated source as default
        // reads. Reuse either form without reopening unchanged workspace files.
        let source = retained.first()?;
        let text = source["text"].as_str()?;
        if retained.len() != 1 || source["status"] != "ok" || source["complete"] != true
            || source["canonical_range"]["start"] != 0
            || source["canonical_range"]["end"] != output["canonical_bytes"]
            || output["canonical_bytes"].as_u64()? != text.len() as u64
        { return None; }
        let selectors: Vec<ToolOutputSelector> = serde_json::from_value(json!(selectors)).ok()?;
        if selectors.iter().any(|selector| !matches!(selector,
            ToolOutputSelector::Bytes { .. } | ToolOutputSelector::Lines { .. } | ToolOutputSelector::Search { .. }
                | ToolOutputSelector::JsonPointer { .. }
        )) { return None; }
        let canonical = file_snapshot(text.to_owned(), Some(&selectors)).ok()?;
        if output["canonical_sha256"].as_str()? != canonical.sha256 { return None; }
        // This replay owner is registered by direct model dispatch only; nested
        // code-mode results do not enter its successful-replay ledger. Match
        // that consumer's policy once instead of projecting in both modes.
        let (selection, continuation) = select_file_snapshot_with_ceiling(
            &canonical, Some(selectors), RECOVERY_AGGREGATE_TOKEN_CEILING.saturating_sub(1_000),
        ).ok()?;
        if !selection.complete || continuation.is_some() { return None; }
        serde_json::to_value(selection.results).ok()?
    } else {
        return None;
    };
    // Ledger candidates may belong to another file or fail to satisfy this
    // selection. Borrow their retained bytes until a reusable selection exists;
    // cloning them during lookup adds work while the caller holds its ledger lock.
    let mut output = output.clone();
    output["results"] = json!(selected);
    output["complete"] = json!(true);
    output["delivered_selection_complete"] = json!(true);
    // Complete inline reads use null instead of a retained artifact identity.
    let mut typed = output.clone();
    if typed["artifact_id"].is_null() { typed["artifact_id"] = json!(""); }
    let typed = serde_json::from_value::<ReadToolOutputResult>(typed).ok()?;
    output["file_complete"] = json!(file_selection_complete(&typed));
    let fields = output.as_object_mut()?;
    for key in ["continuation", "page_selectors", "recovery", "criterion_evidence", "inspection_proof_error", "selector_errors", "selector_bindings"] {
        fields.remove(key);
    }
    Some(output)
}

fn retained_read_selection(result: &serde_json::Value, selector: &serde_json::Value) -> Option<serde_json::Value> {
    if result["status"] != "ok" || result["complete"] != true {
        return None;
    }
    if result["selector"] == *selector
        && (result["text"].is_string() || result["data_base64"].is_string())
    {
        return Some(result.clone());
    }
    // The owning selector engine merges adjacent ranges. A narrower request
    // can still be projected from its exact text, never from line counts alone.
    let text = result["text"].as_str()?;
    let base = result["canonical_range"]["start"].as_u64()?;
    let limit = result["canonical_range"]["end"].as_u64()?;
    if limit.checked_sub(base)? != text.len() as u64
        || result["selector"]["kind"] != selector["kind"]
    { return None; }
    let start = selector["start"].as_u64()?;
    let end = selector["end"].as_u64()?;
    let (start, end) = match selector["kind"].as_str()? {
        "bytes" => (
            usize::try_from(start.checked_sub(base)?).ok()?,
            usize::try_from(end.checked_sub(base)?).ok()?,
        ),
        "lines" => {
            let first = result["selector"]["start"].as_u64()?;
            let start = usize::try_from(start.checked_sub(first)?).ok()?;
            let end = usize::try_from(end.checked_sub(first)?.checked_add(1)?).ok()?;
            let lines = text.split_inclusive('\n').collect::<Vec<_>>();
            lines.get(start..end)?;
            (lines[..start].iter().map(|line| line.len()).sum(),
             lines[..end].iter().map(|line| line.len()).sum())
        }
        _ => return None,
    };
    let text = text.get(start..end)?;
    let mut selected = result.clone();
    selected["selector"] = selector.clone();
    selected["canonical_range"] = json!({"start":base + start as u64, "end":base + end as u64});
    selected["exact_bytes"] = json!(text.len());
    selected["text"] = json!(text);
    Some(selected)
}

impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read_file")
    }

    fn spec(&self) -> ToolSpec {
        let mut selectors = JsonSchema::array(file_selector_schema(), Some("Omit to read the first page immediately. Batch known ranges here. Search results include text in results[].value.hydrated_ranges; enclosing:true also returns enclosing Rust/Python items as line selections. json_pointer selects a complete JSON value. symbol and enclosing share the outline parser; qualified names (Type::method or Class.method) disambiguate items, and broken neighboring syntax is tolerated. Unsupported formats and unresolved items fail explicitly. Ordinary reads do not parse source or generate outlines. Exact source ranges remain recoverable from the snapshot. Oversized selections return child_selectors.".to_string()));
        selectors.min_items = Some(1);
        selectors.max_items = Some(READ_TOOL_OUTPUT_MAX_SELECTORS as u64);
        let mut output = read_tool_output_output_schema(file_selector_schema());
        output["properties"]["path"] = json!({"type": "string"});
        output["properties"]["environment_id"] = json!({"type":"string", "description":"Actual resolved execution environment, or host-skills/host-context for host-owned sources."});
        output["properties"]["canonical_uri"] = json!({"type":"string", "description":"Canonical path URI in the resolved environment, or the host-owned locator."});
        output["properties"]["total_lines"] = json!({"type": "integer", "minimum": 0});
        output["properties"]["source_sha256"] = json!({"type": "string", "description": "SHA-256 of the complete source file, including bytes outside this selection. Use with revision-bound codex-range patch handles."});
        output["properties"]["artifact_id"] = json!({"type": ["string", "null"], "description": "Immutable snapshot identity when retained; omitted or null for complete inline reads or unavailable storage."});
        output["properties"]["canonical_sha256"]["description"] = json!("Compatibility alias for source_sha256; omitted from model presentation when identical.");
        output["properties"]["delivered_selection_complete"]["description"] = json!("Compatibility alias for complete; omitted from model presentation when identical.");
        if let Some(required) = output["required"].as_array_mut() {
            required.retain(|key| !matches!(key.as_str(), Some("artifact_id" | "canonical_sha256" | "delivered_selection_complete")));
            required.push(json!("source_sha256"));
        }
        output["properties"]["file_complete"] = json!({"type": "boolean", "description": "The returned exact bytes and hydrated ranges together cover the entire file in this response, including explicit selections. Retention, match coordinates, and recovery selectors alone do not establish coverage."});
        output["properties"]["selector_bindings"] = json!({"type":"array", "maxItems":64,
            "description":"Successful symbol/enclosing requests bound by their original zero-based selector index to exact snapshot ranges, before result sorting/merging. Resolution is not delivery: check results and completeness separately. Bound to source_sha256; no source is duplicated.",
            "items":{"type":"object", "properties":{
                "selector_index":{"type":"integer","minimum":0},
                "resolved_selectors":{"type":"array","minItems":1,"maxItems":1,"items":file_selector_schema()}},
                "required":["selector_index","resolved_selectors"],"additionalProperties":false}});
        output["properties"]["selector_errors"] = json!({"type":"array", "maxItems":64,
            "description":"Per-selector structural failures; successful results share this snapshot. Candidate line selectors are exact, not guesses.",
            "items":{"type":"object", "properties":{
                "selector_index":{"type":"integer","minimum":0}, "selector":file_selector_schema(),
                "status":{"const":"invalid_selector"}, "complete":{"const":false}, "message":{"type":"string"},
                "source_sha256":{"type":"string"}, "omitted_candidates":{"type":"integer","minimum":0},
                "candidates":{"type":"array","maxItems":8,"items":{"type":"object","properties":{
                    "qualified_name":{"type":"string"}, "selector":file_selector_schema()},
                    "required":["qualified_name","selector"],"additionalProperties":false}}},
                "required":["selector_index","selector","status","complete","message","source_sha256","candidates","omitted_candidates"],
                "additionalProperties":false}});
        output["properties"]["continuation"] =
            serde_json::to_value(file_selector_schema()).unwrap_or_default();
        output["properties"]["recovery"] = json!({
            "type": "object",
            "description": "Ready-to-pass immutable snapshot recovery: call tools.read_tool_output(recovery.arguments). Bounded, deduplicated unconsumed selections; omitted selections remain in per-result continuations/children.",
            "properties": {
                "tool": {"const": "read_tool_output"},
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
            "required": ["tool", "arguments"],
            "additionalProperties": false
        });
        output["properties"]["page_selectors"] = json!({
            "type": "array", "maxItems": 8, "items": file_selector_schema(),
            "description": "First up to eight independent pages of continuation, in source order. Fetch in parallel; continuation retains the full remaining extent."
        });
        output["properties"]["snapshot_error"] = json!({"type": "string", "description": "Snapshot storage failed; inline evidence is still valid, but no recovery handle or continuation is available."});
        output["properties"]["criterion_evidence"] = json!({
            "type": "object",
            "description": "Host-recorded source_inspection proof for a bound typed task requiring inspect:<repository-relative path>. A complete hash-bound read is not proof of semantic correctness.",
            "properties": {
                "call_id": {"type": "string"}, "workspace_id": {"type": "string"},
                "evidence_epoch": {"type": "integer", "minimum": 0},
                "kind": {"const": "source_inspection"}
            },
            "required": ["call_id", "workspace_id", "evidence_epoch", "kind"],
            "additionalProperties": false
        });
        output["properties"]["inspection_proof_error"] = json!({
            "type": "string", "description": "The read completed but no current task proof could be recorded."
        });
        #[expect(
            clippy::expect_used,
            reason = "read_tool_output_output_schema constructs an object with a required array"
        )]
        output["required"]
            .as_array_mut()
            .expect("object schema")
            .extend([json!("path"), json!("total_lines"), json!("file_complete")]);
        ToolSpec::Function(ResponsesApiTool {
            name: "read_file".to_string(),
            description: format!("Read a UTF-8 file without shell quoting. Path alone returns useful text immediately: the whole file if it fits, otherwise its first page plus continuation. Use read_tool_output with the artifact_id and continuation for the remaining immutable snapshot. complete describes delivery of the requested page or explicit selectors; file_complete describes whole-file coverage. Explicit lines (for example start 40, end 90), bytes, or fixed-string search selectors remain exact; check each results[] status. Files may be up to {MAX_FILE_MIB} MiB. In code mode, exact data uses a fixed 1 MiB payload cap independent of the cell's display-token budget; only printed content is display-budgeted. Complete inline reads need no artifact. Snapshot storage failure preserves inline evidence and reports snapshot_error without a recovery handle. Workspace results are freshness-tracked and may reuse dependency-current evidence; force_fresh bypasses replay and reads current contents. Pass a skill: locator to read its SKILL.md; omit environment_id for host-owned skills."),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema {
                one_of: Some(["path", "file_path"].into_iter().map(|field| JsonSchema {
                    required: Some(vec![field.to_string()]),
                    ..JsonSchema::object(BTreeMap::new(), None, None)
                }).collect()),
                ..JsonSchema::object(BTreeMap::from([
                ("path".to_string(), JsonSchema::string(Some("File path, relative to the environment cwd or absolute, a `skill:` locator, `skill:catalog` for enabled skill metadata, `context:desktop` for Desktop guidance, or the `context:unsettled-tools/<offset>` recovery_path from a resume notice. Omit environment_id for host-owned locators.".to_string()))),
                ("file_path".to_string(), JsonSchema::string(Some("Legacy alias for path; use only one.".to_string()))),
                ("offset".to_string(), JsonSchema::integer(Some("Legacy 1-based starting line; use instead of selectors, defaults to 1.".to_string()))),
                ("limit".to_string(), JsonSchema::integer(Some("Legacy positive line count; defaults to 2000 when offset is supplied.".to_string()))),
                ("environment_id".to_string(), JsonSchema::string(Some("Environment id; omit to use the primary environment.".to_string()))),
                ("force_fresh".to_string(), JsonSchema::boolean(Some("Bypass retained-result replay and read current contents, even if dependencies appear unchanged.".to_string()))),
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
            let args: ReadFileArgs = parse_arguments(arguments)?;
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
                    FileSelector::Structure(_) | FileSelector::Exact(ToolOutputSelector::Bytes { .. }
                        | ToolOutputSelector::Lines { .. }
                        | ToolOutputSelector::Search { .. }
                        | ToolOutputSelector::Section { .. }
                        | ToolOutputSelector::JsonPointer { .. })
                )
            }) {
                return Err(FunctionCallError::RespondToModel(
                    "read_file supports bytes, lines, search, section, json_pointer, symbol, and enclosing selectors".to_string(),
                ));
            }
            if invocation.cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "read_file cancelled".to_string(),
                ));
            }
            let turn = &invocation.step_context.turn;
            // The catalog advertises `skill:<id>` locators, so this tool has to
            // resolve them. Without it the model can see every skill listed and
            // load none of them.
            let (contents, resolved_path, inspection, environment_id, canonical_uri) = if args.path == crate::turn_diff_tracker::TURN_DIFF_LOCATOR {
                return Err(FunctionCallError::RespondToModel(
                    "context:turn-diff is not available; tracked deltas are not a reliable review source yet.".to_string(),
                ));
            } else if let Some(offset) = args.path.strip_prefix("context:unsettled-tools/") {
                if args.environment_id.is_some() {
                    return Err(FunctionCallError::RespondToModel(
                        "omit environment_id for host-owned recovery history".into()));
                }
                let offset = offset.parse::<usize>().map_err(|_| FunctionCallError::RespondToModel(
                    "invalid unsettled recovery offset".into()))?;
                let contents = invocation.session.read_unsettled_tool_recovery(offset, &turn.sub_id)
                    .await.map_err(FunctionCallError::RespondToModel)?;
                (contents, args.path.clone(), None, "host-context".to_string(), args.path.clone())
            } else if args.path
                == crate::context::desktop_instructions::LOCATOR
            {
                if args.environment_id.is_some() {
                    return Err(FunctionCallError::RespondToModel(
                        "omit environment_id for host-owned Desktop guidance".to_string(),
                    ));
                }
                let contents = turn.developer_instructions.as_deref()
                    .and_then(crate::context::desktop_instructions::full)
                    .ok_or_else(|| FunctionCallError::RespondToModel(
                        "no Desktop guidance is configured for this turn".to_string(),
                    ))?;
                if contents.len() > MAX_FILE_BYTES {
                    return Err(FunctionCallError::RespondToModel(
                        "Desktop guidance exceeds the read_file size limit".to_string(),
                    ));
                }
                (contents.to_string(), args.path.clone(), None, "host-context".to_string(), args.path.clone())
            } else if args
                .path
                .starts_with(codex_core_skills::SKILL_CATALOG_LOCATOR_PREFIX)
            {
                crate::tools::parallel::wait_for_workspace_baseline().await;
                let (contents, path) = read_skill_locator(&invocation, &args).await?;
                let uri = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&path)
                    .ok().map(|path| codex_utils_path_uri::PathUri::from_abs_path(&path).to_string())
                    .unwrap_or_else(|| args.path.clone());
                (contents, path, None, "host-skills".to_string(), uri)
            } else {
                read_environment_file(&invocation, &args).await?
            };
            if invocation.cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "read_file cancelled".to_string(),
                ));
            }
            let thread_id = invocation.session.thread_id.to_string();
            let explicit_selection = args.selectors.is_some();
            let script_consumer = matches!(&invocation.source, ToolCallSource::CodeMode { .. });
            // Display-sized continuations remain useful for direct model reads.
            // Script selection itself uses a fixed byte cap, not this budget.
            let output_budget = match &invocation.source {
                ToolCallSource::CodeMode { cell_id, .. } => invocation
                    .session.services.code_mode_service.output_budget(cell_id),
                _ => None,
            }.unwrap_or(RECOVERY_AGGREGATE_TOKEN_CEILING);
            // Tiny/zero projections must not disable data processing inside JS.
            // Reserve space for file metadata, page handles, and the exec wrapper.
            let token_ceiling = output_budget.max(2_000).saturating_sub(1_000)
                .min(RECOVERY_AGGREGATE_TOKEN_CEILING.saturating_sub(1_000));
            let structure_path = args.path.clone();
            let (canonical, mut result, mut continuation, page_selectors, total_lines, selector_errors, selector_bindings) =
                tokio::task::spawn_blocking(move || {
                    let total_lines = contents.lines().count();
                    let exact = args.selectors.iter().flatten().filter_map(|selector| match selector {
                        FileSelector::Exact(selector) => Some(selector.clone()),
                        FileSelector::Structure(_) => None,
                    }).collect::<Vec<_>>();
                    let mut canonical = file_snapshot(contents, Some(&exact))?;
                    let (selectors, selector_errors, selector_bindings) = structure::resolve_batch(&structure_path, &mut canonical, args.selectors);
                    let selection = if script_consumer {
                        select_file_snapshot_for_script(&canonical, selectors)
                    } else {
                        select_file_snapshot_with_ceiling(&canonical, selectors, token_ceiling)
                    };
                    selection
                        .map(|(result, continuation)| {
                            let pages = match &continuation {
                                Some(ToolOutputSelector::Bytes { start, end }) =>
                                    crate::tools::command_output_artifact::bounded_page_selectors(
                                        &result.artifact_id, &canonical.bytes, *start, *end,
                                        token_ceiling.min(8_000),
                                    ),
                                _ => Vec::new(),
                            };
                            (canonical, result, continuation, pages, total_lines, selector_errors, selector_bindings)
                        })
                })
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "file selection worker failed: {err}"
                    ))
                })?
                .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
            result.complete &= selector_errors.is_empty();
            let successful = result.selection_status() == "complete";
            let file_complete = selector_errors.is_empty() && file_selection_complete(&result);
            // Explicit selections retain the existing immutable-snapshot contract.
            // Complete default reads stay inline; omitted bytes need one durable
            // snapshot, but selecting them never rereads the file just written.
            let artifact = if explicit_selection || continuation.is_some() {
                let reused = if let Some(id) = invocation.session
                    .reusable_tool_artifact(&invocation.call_id, canonical.exact_bytes, &canonical.sha256).await
                {
                    let artifact = crate::tools::command_output_artifact::attach_canonical_output_artifact(
                        &turn.config.codex_home, &thread_id, &id, &canonical,
                    ).await;
                    artifact.complete.then_some(artifact)
                } else { None };
                Some(match reused {
                    Some(artifact) => artifact,
                    None =>
                    create_canonical_output_artifact(
                        &turn.config.codex_home,
                        &thread_id,
                        &canonical,
                    )
                    .await,
                })
            } else {
                None
            };
            let artifact_id = artifact
                .as_ref()
                .filter(|artifact| artifact.complete)
                .and_then(CanonicalOutputArtifact::artifact_id);
            if let Some(artifact_id) = &artifact_id {
                invocation
                    .session
                    .register_tool_artifact_origin(
                        artifact_id.clone(),
                        invocation.call_id.clone(),
                        canonical.exact_bytes,
                        canonical.sha256.clone(),
                    )
                    .await;
            }
            let snapshot_error =
                artifact
                    .as_ref()
                    .filter(|_| artifact_id.is_none())
                    .map(|artifact| {
                        artifact.error.clone().unwrap_or_else(|| {
                            "unable to retain the complete file snapshot".to_string()
                        })
                    });
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
            let evidence = result.delivered_evidence().map(|identity| {
                let lineage = crate::tools::context::declared_file_lineage_evidence(&canonical.bytes);
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
            output["file_complete"] = json!(file_complete);
            if !selector_errors.is_empty() {
                output["selector_errors"] = json!(selector_errors);
            }
            if !selector_bindings.is_empty() {
                output["selector_bindings"] = json!(selector_bindings);
            }
            if file_complete && let Some(inspection) = inspection {
                match inspection.store.record_source_inspection(
                    inspection.start,
                    invocation.call_id.clone(),
                    canonical.sha256.clone(),
                    canonical.exact_bytes,
                ).await {
                    Ok(reference) => output["criterion_evidence"] = json!(reference),
                    Err(error) => output["inspection_proof_error"] = json!(error.to_string()),
                }
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
            let projected = codex_code_mode::model_visible_tool_result(
                &ToolName::plain("read_file"), &output,
            );
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
            if let Some(projected) = projected {
                output = output.with_model_value(projected);
            }
            Ok(boxed_tool_output(output))
        })
    }
}

// Reuse the recovery engine's lexical boundaries only when explicitly requested.
// Ordinary source reads do not parse JSON or build a new index. Exact source bytes
// (including whitespace and key order) remain the snapshot identity.
fn file_snapshot(
    contents: String,
    selectors: Option<&[ToolOutputSelector]>,
) -> Result<CanonicalToolResult, ReadToolOutputError> {
    let mut canonical = CanonicalToolResult::text(contents);
    if selectors.into_iter().flatten().any(|selector| matches!(selector, ToolOutputSelector::JsonPointer { .. })) {
        // Index failure belongs to JSON selectors, not the raw snapshot or its
        // independent line/byte/search siblings. The selector owner reports it.
        canonical.json_pointers = raw_json_pointer_index(&canonical.bytes).unwrap_or_default();
    }
    Ok(canonical)
}

fn file_recovery_recipe(output: &serde_json::Value, artifact_id: &str) -> Option<serde_json::Value> {
    let mut pending = Vec::new();
    if let Some(continuation) = output.get("continuation").filter(|value| !value.is_null()) {
        pending.push(continuation.clone());
    }
    for result in output["results"].as_array().into_iter().flatten()
        .filter(|result| result["complete"] == false)
    {
        let children = result["child_selectors"].as_array();
        let missing_context = result["selector"]["kind"] == "search"
            && result["status"] == "ok"
            && result["value"]["hydrated_ranges"].as_array().is_some_and(Vec::is_empty);
        if missing_context || result["status"] != "ok" {
            pending.extend(children.into_iter().flatten().cloned());
            // Direct overflow may advertise just the first bounded byte child.
            // Include its unconsumed suffix rather than silently losing it.
            if let Some(end) = result["canonical_range"]["end"].as_u64()
                && let Some(last) = children.into_iter().flatten()
                    .filter(|child| child["kind"] == "bytes")
                    .filter_map(|child| child["end"].as_u64()).max()
                && last < end
            {
                pending.push(json!({"kind":"bytes", "start":last, "end":end}));
            }
        }
        if let Some(continuation) = result.get("continuation").filter(|value| !value.is_null()) {
            if result["selector"]["kind"] == "search" && result["status"] == "ok" {
                // Recover only the undelivered portion of the requested page,
                // not another full page beyond the caller's max_results.
                let requested = result["selector"]["max_results"].as_u64().unwrap_or(20)
                    .min(crate::tools::command_output_artifact::ARTIFACT_SEARCH_MAX_RESULTS as u64)
                    .min(result["value"]["total_matches"].as_u64().unwrap_or(0));
                let remaining = requested.saturating_sub(result["value"]["matches_returned"].as_u64().unwrap_or(0));
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
        if !unique.contains(&selector) { unique.push(selector); }
    }
    let mut selectors = Vec::new();
    let mut bytes = 0usize;
    let mut omitted = 0;
    for selector in unique {
        let size = selector.to_string().len();
        if selectors.len() < READ_TOOL_OUTPUT_MAX_SELECTORS && bytes.saturating_add(size) <= 16 * 1024 {
            bytes += size;
            selectors.push(selector);
        } else {
            omitted += 1;
        }
    }
    if selectors.is_empty() { return None; }
    let mut recipe = json!({"tool":"read_tool_output", "arguments":{"artifact_id":artifact_id, "selectors":selectors}});
    if omitted > 0 { recipe["omitted_selectors"] = json!(omitted); }
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

/// Reads an ordinary path through the selected environment's filesystem.
pub(crate) async fn validate_retained_file_hash(
    step: &crate::session::step_context::StepContext,
    arguments: &str,
    expected_hash: &str,
    expected_bytes: u64,
) -> bool {
    let Ok(args) = parse_arguments::<ReadFileArgs>(arguments) else { return false };
    if args.path.starts_with("skill:") || args.path.starts_with("context:")
        || expected_bytes > MAX_FILE_BYTES as u64
    { return false; }
    let environment = match args.environment_id.as_deref() {
        Some(id) => step.environments.turn_environments.iter().find(|environment| environment.environment_id == id),
        None => step.environments.primary(),
    };
    let Some(environment) = environment.filter(|environment| !environment.environment.is_remote()) else { return false };
    let Ok(path) = environment.cwd().join(&args.path) else { return false };
    let sandbox = step.turn.file_system_sandbox_context(None, environment.cwd());
    let Ok(Some(bytes)) = environment.environment.get_filesystem()
        .read_file_bounded(&path, MAX_FILE_BYTES, Some(&sandbox)).await else { return false };
    if bytes.len() as u64 != expected_bytes { return false; }
    let expected = expected_hash.to_string();
    tokio::task::spawn_blocking(move || crate::tool_history::sha256(&bytes) == expected)
        .await.unwrap_or(false)
}

/// Reads an ordinary path through the selected environment's filesystem.
async fn read_environment_file(
    invocation: &ToolInvocation,
    args: &ReadFileArgs,
) -> Result<(String, String, Option<PendingSourceInspection>, String, String), FunctionCallError> {
    let environment = wait_for_tool_environment(
        &invocation.step_context.environments,
        args.environment_id.as_deref(),
        &invocation.cancellation_token,
    ).await?
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
    let coordinator = invocation.session.services.agent_control.task_coordinator();
    crate::tools::parallel::wait_for_workspace_baseline().await;
    let inspection = if !environment.environment.is_remote()
        && let Some(binding) = coordinator.binding_for_source(&turn.session_source)
        && let Some(store) = coordinator.store()
    {
        store.prepare_source_inspection(binding.attempt_id, path.inferred_native_path_string())
            .await
            .map_err(|error| FunctionCallError::RespondToModel(
                format!("unable to prepare source inspection evidence: {error}")
            ))?
            .map(|start| PendingSourceInspection { store, start })
    } else {
        None
    };
    // Execution backends validate the opened regular file and enforce the
    // byte limit during the stable read. Avoid a separate RPC/helper launch
    // on success; retain metadata only to explain failures as before.
    let result = fs
        .read_file_bounded(&path, MAX_FILE_BYTES, Some(&sandbox))
        .await;
    let contents = match result {
        Ok(Some(contents)) => contents,
        failure => {
            let metadata = match fs.get_metadata(&path, Some(&sandbox)).await {
                Ok(metadata) => metadata,
                Err(err) => {
                    let mut message = format!("unable to locate {}: {err}", path.inferred_native_path_string());
                    // Host-side hints must never probe a remote filesystem or bypass
                    // read restrictions. Failures other than NotFound need no guesses.
                    if err.kind() == std::io::ErrorKind::NotFound
                        && !environment.environment.is_remote()
                        && turn.file_system_sandbox_policy().has_full_disk_read_access()
                        && let (Ok(missing), Ok(cwd)) = (path.to_abs_path(), environment.cwd().to_abs_path())
                    {
                        let suggestions = crate::tools::run_blocking_command_analysis(move || {
                            super::command_search::nearest_existing_paths(missing.as_path(), cwd.as_path())
                        }).await.unwrap_or_default();
                        if !suggestions.is_empty() {
                            message.push_str(&format!("\nPossible existing paths (not verified replacements): {}",
                                suggestions.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", ")));
                        }
                    }
                    return Err(FunctionCallError::RespondToModel(message));
                }
            };
            if metadata.is_directory {
                let listing = fs.walk(&path, codex_exec_server::WalkOptions {
                    max_depth: 0, max_directories: 1, max_entries: 32,
                    follow_directory_symlinks: false, prune_hidden_directories: true,
                    filters: Default::default(),
                }, Some(&sandbox)).await.map_err(|err| {
                    FunctionCallError::RespondToModel(format!("unable to list directory: {err}"))
                })?;
                // This diagnostic promises only immediate entries. Deliberately
                // not descending into child directories does not truncate them.
                let truncated = listing.truncated && (listing.unexplored.is_empty()
                    || listing.unexplored.iter().any(|stop| stop.reason != "depth_limit"));
                return Err(FunctionCallError::RespondToModel(format!(
                    "read_file requires a regular file; this path is a directory. Immediate entries (not file contents): {}. Use a file path, or list_files for a larger listing.",
                    json!({"path":path.inferred_native_path_string(), "entries":listing.entries,
                        "complete": !truncated && listing.errors.is_empty(),
                        "truncated":truncated, "errors":listing.errors})
                )));
            }
            if !metadata.is_file {
                return Err(FunctionCallError::RespondToModel(
                    "read_file requires a regular file".to_string(),
                ));
            }
            if metadata.size > MAX_FILE_BYTES as u64 {
                return Err(FunctionCallError::RespondToModel(format!(
                    "file exceeds the {MAX_FILE_MIB} MiB read limit"
                )));
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
    let contents = String::from_utf8(contents).map_err(|_| {
        FunctionCallError::RespondToModel("read_file requires UTF-8 text".to_string())
    })?;
    Ok((contents, path.inferred_native_path_string(), inspection, environment.environment_id.clone(), path.to_string()))
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
        let mut entries = snapshot.outcome().skills_with_enabled()
            .filter(|(_, enabled)| *enabled)
            .map(|(skill, _)| {
                let locator = format!("skill:{}", codex_core_skills::skill_catalog_id(skill));
                (skill.name.clone(), locator.clone(), json!({
                    "name": skill.name,
                    "description": skill.description,
                    "locator": locator,
                    "implicit_invocation": skill.allows_implicit_invocation(),
                }))
            }).collect::<Vec<_>>();
        entries.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let contents = entries.into_iter().map(|(_, _, entry)| format!("{entry}\n")).collect::<String>();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::tools::command_output_artifact::read_tool_output_selectors_with_reuse;
    use crate::tools::context::ToolCallSource;
    use crate::tools::handlers::ReadToolOutputHandler;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::models::PermissionProfile;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    include!("read_file_navigation_tests.rs");

    #[tokio::test]
    async fn read_identity_and_json_pointer_survive_plain_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        std::fs::write(&path, r#"{"answer":42}"#).unwrap();
        let call = invocation(&path, json!([{"kind":"json_pointer","pointer":"/answer"}]), false).await;
        let output = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        assert_eq!(output["environment_id"], call.step_context.environments.primary().unwrap().environment_id);
        assert_eq!(output["canonical_uri"], codex_utils_path_uri::PathUri::from_abs_path(
            &codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&path).unwrap()).to_string());
        let mut plain = call.clone();
        plain.payload = ToolPayload::Function { arguments: json!({"path":path,"force_fresh":true,"selectors":[{"kind":"bytes","start":0,"end":1}]}).to_string() };
        ReadFileHandler.handle(plain).await.unwrap();
        let (recovered, _) = read_tool_output_selectors_with_reuse(
            &call.step_context.turn.config.codex_home, &call.session.thread_id.to_string(),
            output["artifact_id"].as_str().unwrap(),
            vec![ToolOutputSelector::JsonPointer { pointer: "/answer".into() }],
        ).await.unwrap();
        assert_eq!(recovered.results[0].value, Some(json!(42)));
    }

    #[tokio::test]
    async fn verified10_cold_hash_validation_checks_ignored_bytes_and_link_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ignored.txt");
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(&path, "original").unwrap();
        let call = invocation(&path, json!(null), false).await;
        let arguments = json!({"path":path}).to_string();
        let hash = crate::tool_history::sha256(b"original");
        assert!(validate_retained_file_hash(&call.step_context, &arguments, &hash, 8).await);
        std::fs::write(&path, "modified").unwrap();
        assert!(!validate_retained_file_hash(&call.step_context, &arguments, &hash, 8).await);
        assert!(!validate_retained_file_hash(&call.step_context, &arguments, &hash, 7).await);

        let target = dir.path().join("target.txt");
        std::fs::write(&target, "original").unwrap();
        let link = dir.path().join("source-link.txt");
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&target, &link);
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_file(&target, &link);
        #[cfg(any(unix, windows))]
        {
            if let Err(error) = created {
                // Windows may require symlink privileges unavailable to this fixture.
                if cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied { return; }
                panic!("symlink fixture failed: {error}");
            }
            let arguments = json!({"path":link}).to_string();
            assert!(validate_retained_file_hash(&call.step_context, &arguments, &hash, 8).await);
            std::fs::remove_file(&link).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&path, &link).unwrap();
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(&path, &link).unwrap();
            assert!(!validate_retained_file_hash(&call.step_context, &arguments, &hash, 8).await);
        }
    }

    #[tokio::test]
    async fn code_sections_outline_select_and_recover_original_source() {
        for script in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.rs");
            let source = "// λ\r\n#[test]\r\nfn works() { assert!(true); }\r\nfn other() {}\r\n";
            std::fs::write(&path, source).unwrap();
            let mut call = invocation(&path, json!([{"kind":"section","id":"outline"}]), false).await;
            if script {
                call.source = ToolCallSource::CodeMode { cell_id: "sections".into(), parent_call_id: None,
                    runtime_tool_call_id: "sections-read".into(), nested_deadline: None, cancellation_cause: None };
            }
            let output = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
            assert_eq!(output["complete"], true);
            assert_eq!(output["file_complete"], false);
            let item = &output["results"][0]["value"]["details"]["items"][0];
            assert_eq!(item["name"], "works");
            assert_eq!(item["is_test"], true);
            let id = item["id"].as_str().unwrap().to_owned();
            let selector = ToolOutputSelector::Section { id: id.clone() };
            let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function tool"); };
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap().validate(&output).unwrap();
            let mut selected_call = call.clone();
            selected_call.payload = ToolPayload::Function { arguments: json!({"path":path,"selectors":[selector]}).to_string() };
            let selected = ReadFileHandler.handle(selected_call.clone()).await.unwrap().code_mode_result(&selected_call.payload);
            assert_eq!(selected["results"][0]["text"], "#[test]\r\nfn works() { assert!(true); }");
            // Later plain reads must not erase the section directory on a reused
            // hash-identical artifact, or invalidate already advertised handles.
            let mut plain_call = call.clone();
            plain_call.payload = ToolPayload::Function { arguments: json!({"path":path,"selectors":[{"kind":"lines","start":1,"end":1}]}).to_string() };
            let plain = ReadFileHandler.handle(plain_call.clone()).await.unwrap().code_mode_result(&plain_call.payload);
            assert_ne!(plain["artifact_id"], output["artifact_id"]);
            std::fs::write(&path, "fn replacement() {}\n").unwrap();
            let (recovered, _) = read_tool_output_selectors_with_reuse(
                &call.step_context.turn.config.codex_home, &call.session.thread_id.to_string(),
                output["artifact_id"].as_str().unwrap(), vec![selector],
            ).await.unwrap();
            assert_eq!(recovered.results[0].text.as_deref(), Some("#[test]\r\nfn works() { assert!(true); }"));
            let changed = ReadFileHandler.handle(selected_call.clone()).await.unwrap().code_mode_result(&selected_call.payload);
            assert_eq!(changed["results"][0]["status"], "not_found");
        }
    }

    #[tokio::test]
    async fn missing_file_suggests_existing_sibling_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("spec_plan.rs"), "private contents").unwrap();
        let call = invocation(&dir.path().join("spec.rs"), json!(null), false).await;
        let Err(FunctionCallError::RespondToModel(message)) = ReadFileHandler.handle(call).await else { panic!("missing file"); };
        assert!(message.contains("spec_plan.rs"), "{message}");
        assert!(!message.contains("private contents"));
    }

    #[tokio::test]
    async fn structural_selector_failures_preserve_exact_siblings_and_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambiguous.rs");
        let source = "// λ\r\nimpl A { fn run() {} }\r\nimpl B { fn run() {} }\r\n";
        std::fs::write(&path, source).unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
        let schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            let mut call = invocation(&path, json!([
                {"kind":"lines","start":1,"end":1},
                {"kind":"symbol","name":"run"},
                {"kind":"symbol","name":"missing"},
                {"kind":"symbol","name":"A::run"}
            ]), false).await;
            if script {
                call.source = ToolCallSource::CodeMode { cell_id:"mixed-selectors".into(), parent_call_id:None,
                    runtime_tool_call_id:"mixed-read".into(), nested_deadline:None, cancellation_cause:None };
            }
            let result = ReadFileHandler.handle(call.clone()).await.unwrap();
            assert!(!result.success_for_logging());
            assert!(!result.code_mode_failure_is_error());
            let output = result.code_mode_result(&call.payload);
            schema.validate(&output).unwrap();
            assert_eq!(output["selection_status"], "partial");
            assert_eq!(output["complete"], false);
            assert_eq!(output["file_complete"], false);
            assert!(output["results"].as_array().unwrap().iter().any(|result| result["text"].as_str().is_some_and(|text| text.starts_with("// λ\r\n"))));
            assert_eq!(output["selector_errors"].as_array().unwrap().len(), 2);
            let error = &output["selector_errors"][0];
            assert_eq!(error["selector_index"], 1);
            assert_eq!(error["source_sha256"], output["source_sha256"]);
            assert_eq!(error["candidates"][0]["qualified_name"], "A::run");
            assert_eq!(error["candidates"][1]["selector"], json!({"kind":"lines","start":3,"end":3}));
            assert_eq!(error["omitted_candidates"], 0);
            assert_eq!(output["selector_bindings"], json!([{"selector_index":3,
                "resolved_selectors":[{"kind":"lines","start":2,"end":2}]}]));
        }
    }

    #[tokio::test]
    async fn actionability_symbol_bindings_survive_merging_and_do_not_leak_into_reselection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bindings.rs");
        std::fs::write(&path, "fn first() {}\nfn second() {}\n").unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
        let schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            let mut call = invocation(&path, json!([
                {"kind":"symbol","name":"second"}, {"kind":"symbol","name":"first"}
            ]), false).await;
            if script {
                call.source = ToolCallSource::CodeMode {cell_id:"bindings".into(), parent_call_id:None,
                    runtime_tool_call_id:"bindings".into(), nested_deadline:None, cancellation_cause:None};
            }
            let output = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
            schema.validate(&output).unwrap();
            assert_eq!(output["results"].as_array().unwrap().len(), 1);
            assert_eq!(output["selector_bindings"], json!([
                {"selector_index":0,"resolved_selectors":[{"kind":"lines","start":2,"end":2}]},
                {"selector_index":1,"resolved_selectors":[{"kind":"lines","start":1,"end":1}]}
            ]));
            let ToolPayload::Function {arguments} = &call.payload else { panic!("function"); };
            let requested = json!({"path":path,"selectors":[{"kind":"lines","start":1,"end":1}]}).to_string();
            let reselected = reselect_read_file_output(arguments, &requested, &output).unwrap();
            assert!(reselected.get("selector_bindings").is_none());
            assert_eq!(reselected["source_sha256"], output["source_sha256"]);
        }
    }

    #[tokio::test]
    async fn search_enclosing_and_python_symbols_share_exact_recoverable_items() {
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
        let input_schema = jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap();
        let output_schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for (file, unit, name) in [
            ("a.rs", "/// docs\n#[inline]\nfn target() {\n let needle = 1;\n let x = 2;\n let y = 3;\n let z = 4;\n let _ = needle + x + y + z;\n}\n", "target"),
            ("a.py", "@decorator\ndef target():\n needle = 1\n x = 2\n y = 3\n z = 4\n return needle + x + y + z\n", "target"),
        ] {
            for script in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join(file);
                let source = format!("{unit}\n");
                std::fs::write(&path, &source).unwrap();
                let mut call = invocation(&path, json!([
                    {"kind":"search","query":"NEEDLE","case_insensitive":true,"enclosing":true,"max_results":1,"context_lines":0},
                    {"kind":"section","id":"outline"}
                ]), false).await;
                if script {
                    call.source = ToolCallSource::CodeMode { cell_id: "search-items".into(), parent_call_id: None,
                        runtime_tool_call_id: "search-items-read".into(), nested_deadline: None, cancellation_cause: None };
                }
                let ToolPayload::Function { arguments } = &call.payload else { unreachable!() };
                input_schema.validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap()).unwrap();
                let output = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
                output_schema.validate(&output).unwrap();
                let results = output["results"].as_array().unwrap();
                let search = results.iter().find(|result| result["selector"]["kind"] == "search").unwrap();
                assert_eq!(search["value"]["hydrated_ranges"][0]["text"], unit);
                assert_eq!(search["value"]["enclosing_complete"], true);
                assert_eq!(search["value"]["matches_returned"], 1);
                assert_eq!(search["value"]["remaining_match_count"], 1);
                assert!(search["continuation"]["start_byte"].as_u64().unwrap() > 0);
                let outline = results.iter().find(|result| result["selector"]["kind"] == "section").unwrap();
                assert_eq!(outline["value"]["details"]["items"][0]["start_line"], 1);
                let mut symbol_call = call.clone();
                symbol_call.payload = ToolPayload::Function { arguments: json!({"path":path,"selectors":[{"kind":"symbol","name":name}]}).to_string() };
                let symbol = ReadFileHandler.handle(symbol_call.clone()).await.unwrap().code_mode_result(&symbol_call.payload);
                assert_eq!(symbol["results"][0]["text"], unit);
                std::fs::write(&path, "replacement").unwrap();
                let (recovered, _) = read_tool_output_selectors_with_reuse(
                    &call.step_context.turn.config.codex_home, &call.session.thread_id.to_string(),
                    output["artifact_id"].as_str().unwrap(),
                    vec![ToolOutputSelector::Lines { start: 1, end: unit.lines().count() }],
                ).await.unwrap();
                assert_eq!(recovered.results[0].text.as_deref(), Some(unit));
            }
        }
    }

    #[test]
    fn legacy_read_file_arguments_preserve_line_semantics() {
        let args: ReadFileArgs = parse_arguments(r#"{"file_path":"a.rs","offset":3,"limit":2}"#).unwrap();
        assert_eq!(args.path, "a.rs");
        assert_eq!(args.selectors, Some(vec![FileSelector::Exact(ToolOutputSelector::Lines { start: 3, end: 4 })]));
        for invalid in [
            r#"{"file_path":"a.rs","offset":0}"#,
            r#"{"file_path":"a.rs","limit":0}"#,
            r#"{"file_path":"a.rs","offset":1,"selectors":[]}"#,
        ] {
            assert!(parse_arguments::<ReadFileArgs>(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn rust_units_are_exact_schema_valid_and_recoverable_in_both_consumers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("units.rs");
        let unit = "#[inline]\r\nfn target() {\r\n let s = r#\"λ }\"#;\r\n}\r\n";
        let source = format!("// prefix\r\n{unit}fn other() {{}}\r\n");
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
        let input_schema = jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap();
        let output_schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            for selector in [json!({"kind":"symbol","name":"target"}), json!({"kind":"enclosing","line":4})] {
                std::fs::write(&path, &source).unwrap();
                let mut call = invocation(&path, json!([selector]), false).await;
                if script {
                    call.source = ToolCallSource::CodeMode {
                        cell_id: "rust-unit".into(), parent_call_id: None,
                        runtime_tool_call_id: "rust-unit-read".into(),
                        nested_deadline: None, cancellation_cause: None,
                    };
                }
                let ToolPayload::Function { arguments } = &call.payload else { unreachable!() };
                input_schema.validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap()).unwrap();
                let result = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
                output_schema.validate(&result).unwrap();
                assert_eq!(result["results"][0]["text"], unit);
                assert_eq!(result["results"][0]["selector"], json!({"kind":"lines","start":2,"end":5}));
                assert_eq!(result["complete"], true);
                assert_eq!(result["file_complete"], false);
                std::fs::write(&path, "changed").unwrap();
                let (recovered, _) = read_tool_output_selectors_with_reuse(
                    &call.step_context.turn.config.codex_home, &call.session.thread_id.to_string(),
                    result["artifact_id"].as_str().unwrap(),
                    vec![ToolOutputSelector::Lines { start: 2, end: 5 }],
                ).await.unwrap();
                assert_eq!(recovered.results[0].text.as_deref(), Some(unit));
            }
        }
    }

    #[tokio::test]
    async fn turn_diff_locator_is_not_exposed() {
        let call = invocation(Path::new(crate::turn_diff_tracker::TURN_DIFF_LOCATOR),
            json!([{"kind":"json_pointer","pointer":""}]), true).await;
        let ToolPayload::Function { arguments } = &call.payload else { unreachable!() };
        assert!(canonical_read_file_arguments(arguments).is_none());
        assert!(ReadFileHandler.handle(call).await.is_err());
    }

    #[tokio::test]
    async fn json_pointer_reads_select_complete_units_in_both_consumers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let unit = "{\r\n  \"label\": \"λ😀\", \"items\": [1, {\"enabled\": true}]\r\n}";
        let source = format!("{{\"unrelated\": 99, \"a/b\": {{\"~unit\": {unit}}}}}\r\n");
        let expected: serde_json::Value = serde_json::from_str(unit).unwrap();
        let selectors = json!([{"kind":"json_pointer", "pointer":"/a~1b/~0unit"}]);
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function tool"); };
        let input_schema = jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap();
        let output_schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            std::fs::write(&path, &source).unwrap();
            let mut call = invocation(&path, selectors.clone(), false).await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "json-unit".into(), parent_call_id: None,
                    runtime_tool_call_id: "json-unit-read".into(),
                    nested_deadline: None, cancellation_cause: None,
                };
            }
            let ToolPayload::Function { arguments } = &call.payload else { unreachable!() };
            input_schema.validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap()).unwrap();
            let result = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
            output_schema.validate(&result).unwrap();
            assert_eq!(result["complete"], true);
            assert_eq!(result["file_complete"], false);
            assert_eq!(result["results"][0]["value"], expected);
            assert_eq!(result["source_sha256"], crate::tool_history::sha256(source.as_bytes()));
            let range = &result["results"][0]["canonical_range"];
            let start = range["start"].as_u64().unwrap();
            let end = range["end"].as_u64().unwrap();
            assert_eq!(&source[start as usize..end as usize], unit);

            // A fresh non-pointer selection of identical bytes must not erase
            // addresses already advertised by the earlier artifact.
            let mut plain_call = call.clone();
            plain_call.payload = ToolPayload::Function {
                arguments: json!({"path":path,"force_fresh":true,
                    "selectors":[{"kind":"lines","start":1,"end":1}]}).to_string(),
            };
            let plain = ReadFileHandler.handle(plain_call.clone()).await.unwrap()
                .code_mode_result(&plain_call.payload);
            assert_eq!(plain["complete"], true);
            assert!(plain["artifact_id"].is_string());

            // The known unit boundary and original formatting survive source edits.
            std::fs::write(&path, "replacement").unwrap();
            let (recovered, _) = read_tool_output_selectors_with_reuse(
                &call.step_context.turn.config.codex_home, &call.session.thread_id.to_string(),
                result["artifact_id"].as_str().unwrap(),
                vec![ToolOutputSelector::JsonPointer { pointer: "/a~1b/~0unit".into() },
                    ToolOutputSelector::Bytes { start, end }],
            ).await.unwrap();
            assert!(recovered.complete);
            assert!(recovered.results.iter().any(|r| r.value.as_ref() == Some(&expected)));
            assert!(recovered.results.iter().any(|r| r.text.as_deref() == Some(unit)));
        }
    }

    #[tokio::test]
    async fn json_pointer_reselection_reuses_complete_source_without_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"config":{"enabled":true},"other":null}"#).unwrap();
        let call = invocation(&path, json!(null), false).await;
        let raw = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        let ToolPayload::Function { arguments: previous } = &call.payload else { unreachable!() };
        let requested = json!({"path":path,"selectors":[{"kind":"json_pointer","pointer":"/config"}]}).to_string();
        std::fs::remove_file(&path).unwrap();
        let selected = reselect_read_file_output(previous, &requested, &raw).unwrap();
        assert_eq!(selected["results"][0]["value"], json!({"enabled":true}));
        assert_eq!(selected["complete"], true);
        assert_eq!(selected["file_complete"], false);
        let mut stale = raw;
        stale["canonical_sha256"] = json!("wrong");
        assert!(reselect_read_file_output(previous, &requested, &stale).is_none());
    }

    #[test]
    fn json_pointer_indexing_is_opt_in_and_missing_is_not_invalid() {
        let text = "not JSON\r\n";
        assert!(file_snapshot(text.into(), None).unwrap().json_pointers.is_empty());
        let selectors = vec![ToolOutputSelector::JsonPointer { pointer: "".into() }];
        let invalid = file_snapshot(text.into(), Some(&selectors)).unwrap();
        let (result, _) = select_file_snapshot_for_script(&invalid, Some(selectors.clone())).unwrap();
        assert_eq!(result.results[0].status, crate::tools::command_output_artifact::ToolOutputSelectorStatus::Invalid);
        let canonical = file_snapshot("{\"value\": null}".into(), Some(&selectors)).unwrap();
        let selectors = vec![
            ToolOutputSelector::JsonPointer { pointer: "/missing".into() },
            ToolOutputSelector::JsonPointer { pointer: "invalid".into() },
            ToolOutputSelector::JsonPointer { pointer: "/value".into() },
        ];
        let (result, _) = select_file_snapshot_for_script(&canonical, Some(selectors)).unwrap();
        use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
        for (pointer, status) in [("/missing", ToolOutputSelectorStatus::NotFound),
            ("invalid", ToolOutputSelectorStatus::Invalid), ("/value", ToolOutputSelectorStatus::Ok)] {
            let selected = result.results.iter().find(|result| result.selector ==
                ToolOutputSelector::JsonPointer { pointer: pointer.into() }).unwrap();
            assert_eq!(selected.status, status);
            if pointer == "/value" { assert_eq!(selected.value, Some(json!(null))); }
        }
        assert!(!result.complete);
    }

    #[tokio::test]
    async fn oversized_script_selection_has_drainable_children() {
        let source = "requirement\n".repeat(110_000);
        let canonical = CanonicalToolResult::text(source.clone());
        let parent = ToolOutputSelector::Bytes { start: 0, end: canonical.exact_bytes };
        let (result, _) = select_file_snapshot_for_script(&canonical, Some(vec![parent.clone()])).unwrap();
        let overflow = &result.results[0];
        assert!(!result.complete);
        assert_ne!(overflow.continuation.as_ref(), Some(&parent));
        assert!(!overflow.child_selectors.is_empty());
        let mut recovered = String::new();
        let mut pending = std::collections::VecDeque::from(overflow.child_selectors.clone());
        let mut calls = 0;
        while let Some(selector) = pending.pop_front() {
            calls += 1;
            assert!(calls < 100, "subdivision must make bounded progress");
            assert_ne!(selector, parent);
            let (page, _) = select_file_snapshot_for_script(&canonical, Some(vec![selector.clone()])).unwrap();
            let result = &page.results[0];
            if page.complete {
                assert_eq!(result.canonical_range.unwrap().start, recovered.len() as u64);
                recovered.push_str(result.text.as_ref().unwrap());
            } else {
                assert!(!result.child_selectors.is_empty());
                for child in result.child_selectors.iter().rev() {
                    assert_ne!(child, &selector);
                    pending.push_front(child.clone());
                }
            }
        }
        assert_eq!(recovered, source);
    }

    #[tokio::test]
    async fn script_reads_deliver_all_large_selectors_without_display_budget_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.txt");
        let line = format!("{}\n", "source \"text\" ".repeat(4_000));
        std::fs::write(&path, line.repeat(3)).unwrap();
        let mut call = invocation(&path, json!([
            {"kind": "lines", "start": 1, "end": 1},
            {"kind": "lines", "start": 2, "end": 2},
            {"kind": "lines", "start": 3, "end": 3}
        ]), false).await;
        call.source = ToolCallSource::CodeMode {
            cell_id: "script-read".into(),
            parent_call_id: None,
            runtime_tool_call_id: "script-read-file".into(),
            nested_deadline: None,
            cancellation_cause: None,
        };
        let payload = call.payload.clone();
        let output = ReadFileHandler.handle(call).await.unwrap();
        let result = output.code_mode_result(&payload);
        assert_eq!(result["complete"], true);
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["selector"], json!({"kind":"lines", "start":1, "end":3}));
        assert_eq!(result["results"][0]["text"], line.repeat(3));
    }

    #[tokio::test]
    async fn selected_reads_have_stable_evidence_despite_random_artifact_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();
        let selectors = json!([{"kind":"lines","start":1,"end":1}]);
        let first = ReadFileHandler.handle(invocation(&path, selectors.clone(), false).await).await.unwrap();
        let second = ReadFileHandler.handle(invocation(&path, selectors, false).await).await.unwrap();
        assert!(first.sampling_request_signal().is_some());
        assert_eq!(
            first.sampling_request_signal().unwrap()["semantic_evidence"],
            second.sampling_request_signal().unwrap()["semantic_evidence"],
        );
        let other = ReadFileHandler.handle(invocation(&path, json!([{"kind":"lines","start":2,"end":2}]), false).await).await.unwrap();
        assert_ne!(
            first.sampling_request_signal().unwrap()["semantic_evidence"],
            other.sampling_request_signal().unwrap()["semantic_evidence"],
        );
    }

    #[tokio::test]
    async fn report_reads_keep_native_identity_and_descriptive_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.txt");
        let payload = json!({"paths": ["a.rs", "café.rs"]});
        let lineage = json!({
            "source": "source_inventory",
            "identity": "captured-query-and-source-snapshot",
            // Produced by Python's documented sorted, compact UTF-8 JSON
            // serialization, not by the Rust implementation under test.
            "content_sha256": "0644fbe7bdd8d11f529902ad44409b8ca97d23a60690383de7769744ecb8e975",
        });
        let stdout = json!({"evidence_lineage": lineage});
        let expected = crate::tools::context::declared_lineage_evidence(
            &json!({"evidence_lineage": {
                "source": "source_inventory", "identity": "captured-query-and-source-snapshot"
            }}).to_string().into_bytes(),
        ).unwrap();
        let mut document = payload;
        document["evidence_lineage"] = stdout["evidence_lineage"].clone();
        let body = "Captured paths: a.rs, café.rs\n";
        let mut header = stdout;
        header["evidence_lineage"]["content_sha256"] = json!(crate::tool_history::sha256(body.as_bytes()));
        for report in [
            serde_json::to_string_pretty(&document).unwrap(),
            format!("<!-- codex-evidence: {header} -->\n{body}"),
        ] {
            std::fs::write(&path, &report).unwrap();
            let first = ReadFileHandler.handle(invocation(
                &path, json!([{"kind":"lines", "start":1, "end":100}]), false,
            ).await).await.unwrap();
            let evidence = first.sampling_request_signal().unwrap()["semantic_evidence"].clone();
            assert_eq!(evidence["source"], "read_file");
            assert_eq!(evidence["producer"], expected);
            let mut selections = Vec::new();
            for (start, end) in [(1, 1), (2, 2), (1, 2)] {
                let page = ReadFileHandler.handle(invocation(
                    &path, json!([{"kind":"lines", "start":start, "end":end}]), false,
                ).await).await.unwrap();
                selections.push(page.sampling_request_signal().unwrap()["semantic_evidence"].clone());
            }
            assert_ne!(selections[0], selections[1], "disjoint report pages");
            assert_ne!(selections[0], selections[2], "overlapping report pages");
            assert_ne!(selections[0], expected, "partial report is not whole producer evidence");
            // A stale copied header must not hide an edit to the report.
            std::fs::write(&path, report.replace("a.rs", "b.rs")).unwrap();
            let changed = ReadFileHandler.handle(invocation(
                &path, json!([{"kind":"lines", "start":1, "end":100}]), false,
            ).await).await.unwrap();
            assert_eq!(changed.sampling_request_signal().unwrap()["semantic_evidence"]["source"], "read_file");
            assert_ne!(changed.sampling_request_signal().unwrap()["semantic_evidence"], expected);
        }
        std::fs::write(&path, r#"{"evidence_lineage":{"source":"source_inventory","identity":"old"}}"#).unwrap();
        let unsigned = ReadFileHandler.handle(invocation(
            &path, json!([{"kind":"lines", "start":1, "end":100}]), false,
        ).await).await.unwrap();
        assert_eq!(unsigned.sampling_request_signal().unwrap()["semantic_evidence"]["source"], "read_file");
    }

    #[tokio::test]
    async fn distinct_reports_with_valid_identical_lineage_do_not_collapse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.txt");
        let mut identities = Vec::new();
        for body in ["validation passed", "validation failed"] {
            let header = json!({"evidence_lineage": {
                "source":"report", "scope":"same", "identity":"claimed-same",
                "content_sha256":crate::tool_history::sha256(body.as_bytes())
            }});
            std::fs::write(&path, format!("<!-- codex-evidence: {header} -->\n{body}")).unwrap();
            let output = ReadFileHandler.handle(invocation(
                &path, json!([{"kind":"lines", "start":1, "end":100}]), false,
            ).await).await.unwrap();
            identities.push(output.sampling_request_signal().unwrap()["semantic_evidence"].clone());
        }
        assert_eq!(identities[0]["producer"], identities[1]["producer"]);
        assert_ne!(identities[0]["identity"], identities[1]["identity"]);
    }

    #[tokio::test]
    async fn read_status_resolves_paths_in_the_selected_environment() {
        let mut call = invocation(Path::new("unused"), json!(null), false).await;
        let environment = call.step_context.environments.primary().unwrap();
        let expected = environment.cwd().join("source.rs").unwrap().inferred_native_path_string();
        let environment_id = environment.environment_id.clone();
        call.tool_name = ToolName::plain("read_status");
        call.payload = ToolPayload::Function {
            arguments: json!({"paths":["source.rs"], "environment_id":environment_id}).to_string(),
        };
        let payload = call.payload.clone();
        let output = ReadStatusHandler.handle(call).await.unwrap().code_mode_result(&payload);
        assert_eq!(output["paths"][0]["path"], expected);
        assert_eq!(output["paths"][0]["status"], "unknown");
        let ToolSpec::Function(spec) = ReadStatusHandler.spec() else { panic!("function tool"); };
        let schema = serde_json::to_value(spec.parameters).unwrap();
        assert!(schema["properties"].get("environment_id").is_some());
    }

    async fn invocation(
        path: &Path,
        selectors: serde_json::Value,
        sandboxed: bool,
    ) -> ToolInvocation {
        let (session, mut turn) = make_session_and_context().await;
        turn.permission_profile = if sandboxed {
            PermissionProfile::read_only()
        } else {
            PermissionProfile::Disabled
        };
        ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::new(turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-file".to_string(),
            tool_name: ToolName::plain("read_file"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({"path": path, "selectors": selectors}).to_string(),
            },
        }
    }

    #[tokio::test]
    async fn continuity_unsettled_locator_pages_durable_history_without_reexecution() {
        let mut call = invocation(Path::new("unused"), serde_json::Value::Null, false).await;
        let session = Arc::get_mut(&mut call.session).unwrap();
        let path = crate::session::tests::attach_thread_persistence(session).await;
        let items = (0..20).map(|index| codex_protocol::protocol::RolloutItem::ResponseItem(
            codex_protocol::models::ResponseItem::FunctionCall {
                id: None, call_id: format!("pending-{index:02}"), name: "exec_command".into(),
                namespace: None, arguments: json!({"cmd":"SECRET_COMMAND"}).to_string(),
                internal_chat_message_metadata_passthrough: None,
            })).collect::<Vec<_>>();
        session.persist_rollout_items_durable(&items).await.unwrap();
        session.persist_rollout_items_durable(&[
            codex_protocol::protocol::RolloutItem::TurnContext(call.step_context.turn.to_turn_context_item()),
            items[0].clone(),
        ]).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();
        call.payload = ToolPayload::Function { arguments: json!({"path":"context:unsettled-tools/8"}).to_string() };
        let output = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        let page: serde_json::Value = serde_json::from_str(output["results"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(page["operations"][0]["call_id"], "pending-11");
        assert_eq!(page["unresolved_count"], 20);
        assert_eq!(page["recovery_path"], "context:unsettled-tools/16");
        assert_eq!(page["history_complete"], true);
        assert!(!output.to_string().contains("SECRET_COMMAND"));
        assert_eq!(tokio::fs::read(path).await.unwrap(), before);
        call.payload = ToolPayload::Function { arguments: json!({
            "path":"context:unsettled-tools/16", "environment_id":"local"
        }).to_string() };
        assert!(ReadFileHandler.handle(call).await.is_err());
    }

    #[tokio::test]
    async fn desktop_locator_recovers_full_current_guidance_without_workspace_dependencies() {
        let mut hashes = Vec::new();
        for source in ["original", "updated"] {
            let mut call = invocation(Path::new(crate::context::desktop_instructions::LOCATOR), json!(null), true).await;
            let full = format!("<app-context>\n# Codex desktop context\n### Automations\n{source}\n</app-context>");
            Arc::get_mut(&mut Arc::make_mut(&mut call.step_context).turn)
                .expect("unique fixture turn").developer_instructions =
                Some(format!("private outside\n{full}\nother instructions"));
            assert!(crate::tool_history::source_dependencies_for_tool_call(
                "read_file", &call.payload, Path::new("."),
            ).is_empty());
            let payload = call.payload.clone();
            let output = ReadFileHandler.handle(call).await.unwrap();
            let raw = output.code_mode_result(&payload);
            assert_eq!(raw["file_complete"], true);
            assert_eq!(raw["results"][0]["text"], full);
            assert_eq!(output.sampling_request_signal().unwrap()["semantic_evidence"]["scope"]["environment"], "host-context");
            hashes.push(raw["source_sha256"].clone());
        }
        assert_ne!(hashes[0], hashes[1]);
        let mut missing = invocation(Path::new(crate::context::desktop_instructions::LOCATOR), json!(null), false).await;
        Arc::get_mut(&mut Arc::make_mut(&mut missing.step_context).turn)
            .expect("unique fixture turn").developer_instructions = None;
        assert!(ReadFileHandler.handle(missing).await.is_err());
    }

    #[tokio::test]
    async fn model_projection_compacts_inline_and_retained_reads_without_changing_raw_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projection.txt");
        std::fs::write(&path, "first λ\nsecond 日本語\n").unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function tool"); };
        let validator = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for selectors in [json!(null), json!([{"kind":"lines","start":1,"end":1}]),
            json!([{"kind":"search","query":"first","context_lines":0}])] {
            let call = invocation(&path, selectors.clone(), false).await;
            let payload = call.payload.clone();
            let output = ReadFileHandler.handle(call).await.unwrap();
            let raw = output.code_mode_result(&payload);
            let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output: response, .. } = output.to_response_item("read", &payload) else { panic!("function output"); };
            let codex_protocol::models::FunctionCallOutputBody::Text(text) = response.body else { panic!("text output"); };
            let compact: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(validator.is_valid(&raw));
            validator.validate(&compact).unwrap();
            assert_eq!(raw["source_sha256"], raw["canonical_sha256"]);
            assert_eq!(raw["complete"], raw["delivered_selection_complete"]);
            assert!(compact.get("canonical_sha256").is_none());
            assert!(compact.get("delivered_selection_complete").is_none());
            assert_eq!(compact["source_sha256"], raw["source_sha256"]);
            // Display omits exact_bytes when the canonical range proves it.
            // Reconstruct that alias and compare every field, not just text;
            // scripts must still receive the unchanged raw result contract.
            let mut reconstructed = compact["results"].clone();
            for selected in reconstructed.as_array_mut().unwrap() {
                if selected["selector"]["kind"] == "search" {
                    assert_eq!(selected["value"], raw["results"][0]["value"]);
                    selected["child_selectors"] = json!(selected["value"]["hydrated_ranges"].as_array().unwrap()
                        .iter().map(|range| range["selector"].clone()).collect::<Vec<_>>());
                    continue;
                }
                assert!(selected.get("exact_bytes").is_none());
                let start = selected["canonical_range"]["start"].as_u64().unwrap();
                let end = selected["canonical_range"]["end"].as_u64().unwrap();
                selected["exact_bytes"] = json!(end.checked_sub(start).unwrap());
            }
            assert_eq!(reconstructed, raw["results"]);
            if selectors.is_null() {
                assert!(compact.get("artifact_id").is_none());
                assert_eq!(compact["file_complete"], true);
            } else {
                assert!(compact["artifact_id"].is_string());
                assert_eq!(compact["artifact_id"], raw["artifact_id"]);
                assert_eq!(compact["file_complete"], false);
            }
            assert!(text.len() < raw.to_string().len());
        }
    }

    #[tokio::test]
    async fn explicit_file_coverage_tracks_delivered_bytes_and_preserves_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coverage.txt");
        let text = format!(
            "ALPHA λ {}\r\nMIDDLE 😀\r\nOMEGA tail\r\n",
            "signed evidence -7; ".repeat(20)
        );
        std::fs::write(&path, &text).unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool expected");
        };
        let validator = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for (selectors, expected) in [
            (json!([{"kind":"lines","start":1,"end":3}]), true),
            (json!([{"kind":"bytes","start":0,"end":text.len()}]), true),
            (
                json!([{"kind":"lines","start":1,"end":1},{"kind":"lines","start":2,"end":3}]),
                true,
            ),
            (
                json!([{"kind":"search","query":"ALPHA","context_lines":2}]),
                true,
            ),
            (
                json!([{"kind":"search","query":"ALPHA","context_lines":2},{"kind":"search","query":"OMEGA","context_lines":2}]),
                true,
            ),
            (
                json!([{"kind":"search","query":"ALPHA"},{"kind":"search","query":"MIDDLE"},{"kind":"search","query":"OMEGA"}]),
                true,
            ),
            (
                json!([{"kind":"lines","start":1,"end":1},{"kind":"lines","start":3,"end":3}]),
                false,
            ),
            (json!([{"kind":"bytes","start":1,"end":text.len()}]), false),
            (json!([{"kind":"search","query":"absent"}]), false),
            (json!([{"kind":"lines","start":99,"end":100}]), false),
        ] {
            let call = invocation(&path, selectors.clone(), false).await;
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            assert_eq!(result["file_complete"], expected, "{selectors}: {result}");
            if selectors.as_array().unwrap().len() == 2 && selectors[0]["context_lines"] == 2 {
                assert_eq!(
                    result["results"][1]["value"]["hydrated_ranges"][0]["shared"],
                    true
                );
            }
            validator.validate(&result).unwrap();
            let snapshot = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
                &call.step_context.turn.config.codex_home,
                &call.session.thread_id.to_string(),
                result["artifact_id"].as_str().unwrap(),
                MAX_FILE_BYTES,
            )
            .await
            .unwrap();
            assert_eq!(snapshot, text.as_bytes());
        }
        // An empty exact result really covers an empty file; empty search hits do not.
        std::fs::write(&path, "").unwrap();
        for (selectors, expected) in [
            (json!([{"kind":"bytes","start":0,"end":0}]), true),
            (json!([{"kind":"search","query":"absent"}]), false),
        ] {
            let call = invocation(&path, selectors, false).await;
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            assert_eq!(result["file_complete"], expected);
        }
        // Requested coordinates and retained bytes must not masquerade as delivered evidence.
        std::fs::write(&path, "ALPHA evidence\r\n".repeat(20_000)).unwrap();
        let call = invocation(
            &path,
            json!([{"kind":"lines","start":1,"end":20_000}]),
            false,
        )
        .await;
        let result = ReadFileHandler
            .handle(call.clone())
            .await
            .unwrap()
            .code_mode_result(&call.payload);
        assert_eq!(result["file_complete"], false);
        assert_eq!(result["complete"], false);
        assert_eq!(result["retained_artifact_complete"], true);
        assert!(result["results"][0]["text"].is_null());
    }

    #[tokio::test]
    async fn complete_explicit_read_does_not_depend_on_snapshot_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coverage.txt");
        std::fs::write(&path, "complete evidence\n").unwrap();
        let blocked_home = dir.path().join("blocked");
        std::fs::write(&blocked_home, "not a directory").unwrap();
        let mut call = invocation(&path, json!([{"kind":"lines","start":1,"end":1}]), false).await;
        let step = Arc::get_mut(&mut call.step_context).unwrap();
        let turn = Arc::get_mut(&mut step.turn).unwrap();
        Arc::make_mut(&mut turn.config).codex_home =
            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&blocked_home).unwrap();
        let result = ReadFileHandler
            .handle(call.clone())
            .await
            .unwrap()
            .code_mode_result(&call.payload);
        assert_eq!(result["file_complete"], true);
        assert_eq!(result["results"][0]["text"], "complete evidence\n");
        assert_eq!(result["retained_artifact_complete"], false);
        assert!(result["snapshot_error"].is_string());
        assert!(result["artifact_id"].is_null());
        assert!(result["continuation"].is_null());
    }

    #[tokio::test]
    async fn registered_read_file_records_evidence_and_invalidates_after_changes() {
        use crate::session::turn_context::TurnEnvironment;
        use crate::tool_history::SourceDependencyV1;
        use crate::tools::parallel::ToolCallRuntime;
        use crate::tools::registry::ToolRegistry;
        use crate::tools::router::ToolCall;
        use crate::tools::router::ToolRouter;
        use codex_protocol::models::ResponseItem;
        use codex_utils_absolute_path::AbsolutePathBuf;
        use codex_utils_path_uri::PathUri;
        use std::collections::BTreeSet;

        let workspace = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(workspace.path())
                .status()
                .unwrap()
                .success()
        );
        let path = workspace.path().join("file.txt");
        std::fs::write(&path, "current text\n").unwrap();
        let (session, mut turn) = make_session_and_context().await;
        Arc::make_mut(&mut turn.config).cwd =
            AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap();
        turn.permission_profile = PermissionProfile::Disabled;
        turn.environments.turn_environments = vec![TurnEnvironment::new(
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            Arc::new(codex_exec_server::Environment::default_for_tests()),
            PathUri::from_host_native_path(workspace.path()).unwrap(),
            None,
        )];
        let session = Arc::new(session);
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([Arc::new(ReadFileHandler) as Arc<dyn CoreToolRuntime>]),
            Vec::new(),
        ));
        let step = StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            session.clone(),
            step,
            Arc::new(Mutex::new(TurnDiffTracker::new())),
        );
        let arguments = json!({"path": "file.txt"}).to_string();
        let result = runtime
            .clone()
            .handle_tool_call_with_source(
                ToolCall {
                    tool_name: ToolName::plain("read_file"),
                    call_id: "registered-file-read".into(),
                    payload: ToolPayload::Function {
                        arguments: arguments.clone(),
                    },
                },
                ToolCallSource::CodeMode {
                    cell_id: "read-cell".into(),
                    parent_call_id: Some("outer-exec".into()),
                    runtime_tool_call_id: "runtime-read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.projected_source_dependencies(),
            Some(&BTreeSet::from([SourceDependencyV1::new(&path, false)]))
        );
        let canonical: Arc<[ResponseItem]> = Arc::from([
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".into(),
                namespace: None,
                arguments,
                call_id: "registered-file-read".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::from(result.response()),
        ]);
        // `code_mode_result` consumes the result, so project it once.
        let code_mode_result = result.code_mode_result();
        assert_eq!(code_mode_result["results"][0]["text"], "current text\n");
        assert_eq!(code_mode_result["file_complete"], true);
        assert_eq!(code_mode_result["artifact_id"], serde_json::Value::Null);
        let mut history = session.clone_history().await.tool_history_state();
        let revision = history
            .workspace_evidence_revision_for_test("registered-file-read")
            .unwrap();
        assert_eq!(
            history
                .project_with_workspace_cache(
                    canonical.clone(),
                    revision.as_ref(),
                    &session.services.git_workspace
                )
                .items,
            canonical
        );
        std::fs::write(&path, "changed text\n").unwrap();
        assert!(
            history
                .invalidate_source_dependencies(Some(&BTreeSet::from([path])), revision.as_ref())
        );
        let projected = history.project_with_workspace_identity(canonical, revision.as_ref());
        let ResponseItem::FunctionCallOutput { output, .. } = &projected.items[1] else {
            panic!("expected stale read output");
        };
        let notice: serde_json::Value =
            serde_json::from_str(&output.body.to_text().unwrap()).unwrap();
        assert_eq!(notice["reason_code"], "source_dependencies_invalidated");
        assert_eq!(notice["rerun"]["tool"], "read_file");
        let retry = notice["rerun"]["arguments"].clone();
        assert_eq!(retry, json!({"path": "file.txt"}));
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("expected callable read schema");
        };
        let parameters = serde_json::to_value(spec.parameters).unwrap();
        assert!(
            jsonschema::validator_for(&parameters)
                .unwrap()
                .is_valid(&retry)
        );
        let recovered = runtime
            .handle_tool_call_with_source(
                ToolCall {
                    tool_name: ToolName::plain(notice["rerun"]["tool"].as_str().unwrap()),
                    call_id: "recovered-file-read".into(),
                    payload: ToolPayload::Function {
                        arguments: retry.to_string(),
                    },
                },
                ToolCallSource::Direct,
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .code_mode_result();
        assert_eq!(recovered["results"][0]["text"], "changed text\n");
        assert_eq!(recovered["file_complete"], true);
    }

    #[tokio::test]
    async fn omitted_selectors_deliver_text_before_recovery_and_skip_unneeded_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("whole.txt");
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool expected");
        };
        let parameters = serde_json::to_value(&spec.parameters).unwrap();
        let validator = jsonschema::validator_for(&parameters).unwrap();
        for text in [
            String::new(),
            "one\nλ two\n".to_string(),
            "large line\n".repeat(10_000),
        ] {
            std::fs::write(&path, &text).unwrap();
            let mut call = invocation(&path, json!([]), false).await;
            let arguments = json!({"path": path});
            assert!(validator.is_valid(&arguments));
            call.payload = ToolPayload::Function {
                arguments: arguments.to_string(),
            };
            let artifact_directory = call
                .step_context
                .turn
                .config
                .codex_home
                .join("tool-output")
                .join(call.session.thread_id.to_string());
            let payload = call.payload.clone();
            let result = ReadFileHandler
                .handle(call)
                .await
                .unwrap()
                .code_mode_result(&payload);
            assert_eq!(result["canonical_bytes"], text.len());
            assert_eq!(
                result["source_sha256"],
                CanonicalToolResult::text(text.clone()).sha256
            );
            assert_eq!(result["total_lines"], text.lines().count());
            let delivered = result["results"][0]["text"].as_str().unwrap();
            assert!(text.starts_with(delivered));
            assert_eq!(result["complete"], true);
            assert_eq!(result["results"][0]["status"], "ok");
            assert_eq!(
                result["results"][0]["selector"],
                json!({"kind": "bytes", "start": 0, "end": delivered.len()})
            );
            if text.len() < 100 {
                assert_eq!(delivered, text);
                assert_eq!(result["file_complete"], true);
                assert!(result["artifact_id"].is_null());
                assert_eq!(result["retained_artifact_complete"], false);
                assert!(
                    !artifact_directory.exists(),
                    "inline reads must not write snapshots"
                );
            } else {
                assert!(!delivered.is_empty());
                assert!(delivered.len() < text.len());
                assert_eq!(result["file_complete"], false);
                assert_eq!(
                    result["continuation"],
                    json!({"kind": "bytes", "start": delivered.len(), "end": text.len()})
                );
                assert!(result["artifact_id"].as_str().is_some());
            }
            jsonschema::validator_for(&spec.output_schema.as_ref().unwrap().to_value())
                .unwrap()
                .validate(&result)
                .unwrap();
        }
        assert!(!validator.is_valid(&json!({"path": path, "selectors": []})));
    }

    #[tokio::test]
    async fn nested_file_pages_ignore_display_budget_and_recover_original_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounded.txt");
        let original = "λ exact immutable source\r\n".repeat(60_000);
        std::fs::write(&path, &original).unwrap();
        let mut call = invocation(&path, json!(null), false).await;
        let cell = codex_code_mode::CellId::new("bounded-file-read".into());
        call.session.services.code_mode_service.record_cell_parent_call_id(&cell, "outer");
        call.source = ToolCallSource::CodeMode {
            cell_id: cell.to_string(),
            parent_call_id: Some("outer".into()),
            runtime_tool_call_id: "read".into(),
            nested_deadline: None,
            cancellation_cause: None,
        };
        let canonical = CanonicalToolResult::text(original.clone());
        let (unbounded, _) =
            crate::tools::command_output_artifact::select_file_snapshot(&canonical, None).unwrap();
        assert!(codex_utils_string::approx_token_count(
            &serde_json::to_string(&unbounded).unwrap()
        ) > 4_000, "the old fixed-size page would exceed the owner budget");
        let mut retained_page = None;
        for budget in [0, 512, 4_000, 10_000] {
            call.session.services.code_mode_service.record_output_budget(&cell, Some(budget));
            assert_eq!(call.session.services.code_mode_service.output_budget(cell.as_str()), Some(budget));
            let output = ReadFileHandler.handle(call.clone()).await.unwrap();
            let result = output.code_mode_result(&call.payload);
            assert!(
                result.to_string().len() <= 1024 * 1024,
                "the page and recovery handles must fit the script payload cap: {budget}"
            );
            let delivered = result["results"][0]["text"].as_str().unwrap();
            assert!(!delivered.is_empty());
            assert!(original.starts_with(delivered));
            assert_eq!(result["complete"], true);
            assert_eq!(result["file_complete"], false);
            assert_eq!(result["source_sha256"], canonical.sha256);
            assert_eq!(
                result["continuation"],
                json!({"kind": "bytes", "start": delivered.len(), "end": original.len()})
            );
            assert_eq!(result["page_selectors"][0]["start"], delivered.len());
            if budget == 4_000 {
                retained_page = Some(result);
            }
        }
        std::fs::write(&path, "changed after the original read\n").unwrap();
        let retained = retained_page.unwrap();
        let selector = retained["page_selectors"][0].clone();
        call.session.services.code_mode_service.record_output_budget(&cell, Some(4_000));
        call.tool_name = ToolName::plain("read_tool_output");
        call.payload = ToolPayload::Function {
            arguments: json!({
                "artifact_id": retained["artifact_id"],
                "selectors": [selector],
            }).to_string(),
        };
        let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap()
            .code_mode_result(&call.payload);
        assert_eq!(
            recovered["complete"], true,
            "requested={selector}; stop={}; results={:?}",
            recovered["continuation_stop"],
            recovered["results"].as_array().unwrap().iter()
                .map(|result| (&result["status"], &result["canonical_range"], &result["continuation"]))
                .collect::<Vec<_>>()
        );
        let start = selector["start"].as_u64().unwrap() as usize;
        let end = selector["end"].as_u64().unwrap() as usize;
        assert_eq!(recovered["results"][0]["text"], &original[start..end]);
        assert!(codex_utils_string::approx_token_count(&recovered.to_string()) <= 4_000);

        // The ready-to-pass recipe preserves the script-sized budget. The
        // serialized envelope may still require multiple exact recovery pages.
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
        jsonschema::validator_for(&spec.output_schema.unwrap().to_value())
            .unwrap().validate(&retained).unwrap();
        let mut recipe = retained["recovery"]["arguments"].clone();
        assert_eq!(recipe["max_bytes"], READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES);
        let start = retained["continuation"]["start"].as_u64().unwrap() as usize;
        let mut offset = start;
        let mut tail = String::new();
        let mut page_recipe = recipe.clone();
        for page_index in 0..64 {
            call.payload = ToolPayload::Function { arguments: page_recipe.to_string() };
            let page = ReadToolOutputHandler.handle(call.clone()).await.unwrap()
                .code_mode_result(&call.payload);
            assert_eq!(page["canonical_sha256"], retained["source_sha256"]);
            assert!(page.to_string().len() <= READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES);
            let before = offset;
            for part in page["results"].as_array().unwrap() {
                if let Some(text) = part["text"].as_str() {
                    assert_eq!(part["status"], "ok");
                    assert_eq!(part["canonical_range"]["start"], offset);
                    offset += text.len();
                    assert_eq!(part["canonical_range"]["end"], offset);
                    tail.push_str(text);
                }
            }
            assert!(offset > before, "recovery must make progress");
            if page_index == 0 {
                assert!(offset - before > 16_384, "recipe must retain the script-sized budget");
            }
            if page["complete"] == true {
                assert_eq!(offset, original.len());
                break;
            }
            let stop = &page["continuation_stop"];
            assert_eq!(stop["reason"], "budget");
            assert_eq!(stop["resumable"], true);
            assert_eq!(stop["selector"], json!({"kind":"bytes", "start":offset, "end":original.len()}));
            page_recipe["selectors"] = json!([stop["selector"].clone()]);
        }
        assert_eq!(offset, original.len());
        assert_eq!(tail, original[start..]);

        // Same requested suffix, old recipe: another recovery is necessary.
        recipe.as_object_mut().unwrap().remove("max_bytes");
        call.payload = ToolPayload::Function { arguments: recipe.to_string() };
        let paged = ReadToolOutputHandler.handle(call.clone()).await.unwrap()
            .code_mode_result(&call.payload);
        assert_eq!(paged["complete"], false);
        assert_eq!(paged["continuation_stop"]["reason"], "budget");
        assert_eq!(paged["continuation_stop"]["resumable"], true);
    }

    #[test]
    fn file_page_sizing_measures_delivered_utf8_bytes() {
        let canonical = CanonicalToolResult::text("λ exact immutable source\r\n".repeat(6_000));
        let artifact_id = uuid::Uuid::nil().to_string();
        for ceiling in [1_000, 2_000, 3_000] {
            let (first, _) = select_file_snapshot_with_ceiling(&canonical, None, ceiling).unwrap();
            assert!(codex_utils_string::approx_token_count(
                &serde_json::to_string(&first).unwrap()
            ) <= ceiling);
            for selector in crate::tools::command_output_artifact::bounded_page_selectors(
                &artifact_id, &canonical.bytes, 0, canonical.exact_bytes, ceiling,
            ) {
                let selected = crate::tools::command_output_artifact::select_producer_snapshot(
                    &canonical, &artifact_id, vec![selector], ceiling,
                ).unwrap();
                assert!(selected.complete, "every advertised page must fit: {selected:?}");
                let page = &selected.results[0];
                let range = page.canonical_range.unwrap();
                assert_eq!(
                    page.text.as_ref().unwrap().as_bytes(),
                    &canonical.bytes[range.start as usize..range.end as usize]
                );
            }
        }
    }

    #[tokio::test]
    async fn nested_file_explicit_ranges_preserve_exactness_under_script_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selected.txt");
        let original = "exact requested source line\n".repeat(60_000);
        std::fs::write(&path, &original).unwrap();
        let cell = codex_code_mode::CellId::new("bounded-explicit-read".into());
        for end in [1_000, 16_000, original.len()] {
            let selector = json!({"kind": "bytes", "start": 0, "end": end});
            let mut call = invocation(&path, json!([selector]), false).await;
            call.session.services.code_mode_service.record_cell_parent_call_id(&cell, "outer");
            call.source = ToolCallSource::CodeMode {
                cell_id: cell.to_string(),
                parent_call_id: Some("outer".into()),
                runtime_tool_call_id: "selected-read".into(),
                nested_deadline: None,
                cancellation_cause: None,
            };
            call.session.services.code_mode_service.record_output_budget(&cell, Some(4_000));
            let result = ReadFileHandler.handle(call.clone()).await.unwrap()
                .code_mode_result(&call.payload);
            assert!(result.to_string().len() <= 1024 * 1024);
            assert_eq!(result["results"][0]["selector"], selector);
            if end <= 16_000 {
                assert_eq!(result["complete"], true);
                assert_eq!(result["results"][0]["text"], &original[..end]);
            } else {
                assert_eq!(result["complete"], false);
                assert_eq!(result["file_complete"], false);
                assert!(result["artifact_id"].is_string());
                let first = &result["results"][0];
                assert_eq!(first["status"], "aggregate_omitted");
                let child_end = first["continuation"]["end"].as_u64().unwrap() as usize;
                assert!(child_end > 0 && child_end < end);
                assert_eq!(first["continuation"]["start"], 0);
                assert_eq!(first["child_selectors"][1], json!({"kind":"bytes", "start":child_end, "end":end}));
            }
        }
    }

    #[tokio::test]
    async fn identical_files_keep_distinct_provenance_through_edit_and_restart() {
        use crate::tool_history::{ToolHistoryLoadOutcome, WorkspaceEvidenceObservation, SourceDependencyV1};
        use std::collections::BTreeSet;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "identical source\n").unwrap();
        std::fs::write(&b, "identical source\n").unwrap();
        let mut call_a = invocation(&a, json!([{"kind":"lines", "start":1, "end":1}]), false).await;
        call_a.call_id = "file-a".into();
        let mut call_b = call_a.clone();
        call_b.call_id = "file-b".into();
        call_b.payload = ToolPayload::Function { arguments:json!({"path":b,
            "selectors":[{"kind":"lines", "start":1, "end":1}]}).to_string() };
        let output_a = ReadFileHandler.handle(call_a.clone()).await.unwrap();
        let output_b = ReadFileHandler.handle(call_b.clone()).await.unwrap();
        let a_value = output_a.code_mode_result(&call_a.payload);
        let b_value = output_b.code_mode_result(&call_b.payload);
        assert_ne!(a_value["artifact_id"], b_value["artifact_id"]);
        let mut history = call_a.session.lock_history_state_for_test().await.tool_history_state();
        for (call, output, path) in [(&call_a, &output_a, &a), (&call_b, &output_b, &b)] {
            let item = output.to_response_item(&call.call_id, &call.payload).into();
            history.register_workspace_evidence(WorkspaceEvidenceObservation::from_response_item(
                None, &item, BTreeSet::from([SourceDependencyV1::new(path, false)])).unwrap());
        }
        let home = &call_a.step_context.turn.config.codex_home;
        let thread = call_a.session.thread_id.to_string();
        let id = a_value["artifact_id"].as_str().unwrap();
        for edited in [false, true] {
            if edited {
                std::fs::write(&a, "changed A only\n").unwrap();
                assert!(history.invalidate_source_dependencies(Some(&BTreeSet::from([a.clone()])), None));
            }
            for restarted in [false, true] {
                if restarted {
                    crate::tool_history::persist_tool_history_state(home, &thread, &history).await.unwrap();
                    let ToolHistoryLoadOutcome::Loaded(restored) = crate::tool_history::load_tool_history_state(home, &thread).await else { panic!("history restart") };
                    history = restored;
                }
                let provenance = serde_json::to_value(&history).unwrap();
                assert_eq!(provenance["internal_artifact_origins"][id][0], "file-a");
                assert_eq!(provenance["workspace_evidence"]["file-a"]["source_dependencies_current"], !edited);
                assert_eq!(provenance["workspace_evidence"]["file-b"]["source_dependencies_current"], true);
                let (recovered, _) = read_tool_output_selectors_with_reuse(home, &thread, id,
                    vec![ToolOutputSelector::Lines { start:1, end:1 }]).await.unwrap();
                assert_eq!(recovered.results[0].text.as_deref(), Some("identical source\n"));
            }
        }
    }

    #[tokio::test]
    async fn default_page_continuation_recovers_utf8_crlf_snapshot_after_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pages.txt");
        let original = "λ first\r\n😀 second\r\n".repeat(6_000);
        std::fs::write(&path, &original).unwrap();
        let mut call = invocation(&path, json!([]), false).await;
        call.payload = ToolPayload::Function {
            arguments: json!({"path": path}).to_string(),
        };
        let result = ReadFileHandler
            .handle(call.clone())
            .await
            .unwrap()
            .code_mode_result(&call.payload);
        let artifact_id = result["artifact_id"].as_str().unwrap();
        let history = call
            .session
            .lock_history_state_for_test()
            .await
            .tool_history_state();
        assert_eq!(
            history.artifact_references().get(artifact_id),
            Some(&(
                original.len() as u64,
                crate::tool_history::sha256(original.as_bytes())
            ))
        );
        let provenance = serde_json::to_value(history).unwrap();
        assert_eq!(
            provenance["internal_artifact_origins"][artifact_id][0],
            call.call_id
        );

        let mut recovered = result["results"][0]["text"]
            .as_str()
            .unwrap()
            .as_bytes()
            .to_vec();
        let mut selector = result["continuation"].clone();
        assert_eq!(result["recovery"], json!({
            "tool": "read_tool_output",
            "arguments": {"artifact_id": artifact_id, "selectors": [selector.clone()]}
        }));
        let mut recovery_arguments = result["recovery"]["arguments"].clone();
        assert!(!recovered.is_empty());
        assert!(original.as_bytes().starts_with(&recovered));
        std::fs::write(&path, "changed after the first page\n").unwrap();
        for _ in 0..100 {
            if selector.is_null() {
                break;
            }
            let mut recovery_call = call.clone();
            recovery_call.tool_name = ToolName::plain("read_tool_output");
            recovery_call.payload = ToolPayload::Function {
                arguments: recovery_arguments.to_string(),
            };
            let output = ReadToolOutputHandler
                .handle(recovery_call.clone())
                .await
                .unwrap()
                .code_mode_result(&recovery_call.payload);
            assert_eq!(output["canonical_sha256"], result["canonical_sha256"]);
            let before = recovered.len();
            for fragment in output["results"].as_array().unwrap() {
                let bytes = if let Some(text) = fragment["text"].as_str() {
                    Some(text.as_bytes().to_vec())
                } else {
                    use base64::Engine;
                    fragment["data_base64"].as_str().map(|data| {
                        base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .unwrap()
                    })
                };
                if let Some(bytes) = bytes {
                    assert_eq!(
                        fragment["canonical_range"]["start"].as_u64(),
                        Some(recovered.len() as u64)
                    );
                    recovered.extend_from_slice(&bytes);
                    assert_eq!(
                        fragment["canonical_range"]["end"].as_u64(),
                        Some(recovered.len() as u64)
                    );
                }
            }
            assert!(
                recovered.len() > before,
                "every recovery must deliver new source bytes: {output}"
            );
            selector = output["continuation_stop"]["selector"].clone();
            recovery_arguments = json!({"artifact_id": artifact_id, "selectors": [selector.clone()]});
        }
        assert!(selector.is_null(), "recovery must terminate");
        assert_eq!(
            recovered.len(),
            original.len(),
            "recovery must retain the entire suffix"
        );
        assert_eq!(String::from_utf8(recovered).unwrap(), original);
    }

    #[tokio::test]
    async fn snapshot_storage_failure_preserves_inline_evidence_without_dangling_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.txt");
        let text = "readable source\n".repeat(10_000);
        std::fs::write(&path, &text).unwrap();
        let blocked_home = dir.path().join("not-a-directory");
        std::fs::write(&blocked_home, "blocked").unwrap();
        for selectors in [None, Some(json!([{"kind": "lines", "start": 1, "end": 1}]))] {
            let mut call = invocation(&path, json!([]), false).await;
            let step = Arc::get_mut(&mut call.step_context).unwrap();
            let turn = Arc::get_mut(&mut step.turn).unwrap();
            Arc::make_mut(&mut turn.config).codex_home =
                codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&blocked_home)
                    .unwrap();
            let mut args = json!({"path": path});
            if let Some(selectors) = selectors {
                args["selectors"] = selectors;
            }
            call.payload = ToolPayload::Function {
                arguments: args.to_string(),
            };
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            let delivered = result["results"][0]["text"].as_str().unwrap();
            assert!(!delivered.is_empty());
            assert!(text.starts_with(delivered));
            assert_eq!(result["complete"], true);
            assert_eq!(result["file_complete"], false);
            assert!(result["artifact_id"].is_null());
            assert!(result["snapshot_error"].as_str().is_some());
            assert!(result["continuation"].is_null());
            assert_eq!(result["retained_bytes"], 0);
            assert_eq!(result["retained_artifact_complete"], false);
            let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
                panic!("function tool")
            };
            jsonschema::validator_for(&spec.output_schema.as_ref().unwrap().to_value())
                .unwrap()
                .validate(&result)
                .unwrap();
        }
    }

    #[tokio::test]
    async fn selected_lines_recover_the_original_snapshot_after_a_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a file's contents.txt");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let call = invocation(
            &path,
            json!([{"kind": "lines", "start": 2, "end": 2}]),
            false,
        )
        .await;
        let codex_home = call.step_context.turn.config.codex_home.clone();
        let thread_id = call.session.thread_id.to_string();
        let payload = call.payload.clone();
        let result = ReadFileHandler
            .handle(call)
            .await
            .unwrap()
            .code_mode_result(&payload);
        assert_eq!(result["results"][0]["text"], "second\n");
        assert_eq!(result["complete"], true);
        assert_eq!(result["path"], path.to_string_lossy().as_ref());
        assert_eq!(result["canonical_bytes"], 19);
        assert_eq!(result["total_lines"], 3);
        assert_eq!(result["canonical_sha256"].as_str().unwrap().len(), 64);
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool expected")
        };
        jsonschema::validator_for(&spec.output_schema.as_ref().unwrap().to_value())
            .unwrap()
            .validate(&result)
            .unwrap();

        std::fs::write(&path, "replacement\n").unwrap();
        let (recovered, _) = read_tool_output_selectors_with_reuse(
            &codex_home,
            &thread_id,
            result["artifact_id"].as_str().unwrap(),
            vec![ToolOutputSelector::Lines { start: 1, end: 3 }],
        )
        .await
        .unwrap();
        assert_eq!(
            recovered.results[0].text.as_deref(),
            Some("first\nsecond\nthird\n")
        );
        assert_eq!(
            recovered.canonical_sha256,
            result["canonical_sha256"].as_str().unwrap()
        );
    }

    #[tokio::test]
    async fn bytes_and_search_return_exact_file_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.txt");
        std::fs::write(&path, "before\nneedle\nafter\n").unwrap();
        let call = invocation(
            &path,
            json!([
                {"kind": "bytes", "start": 0, "end": 6},
                {"kind": "search", "query": "needle", "context_lines": 1}
            ]),
            false,
        )
        .await;
        let payload = call.payload.clone();
        let result = ReadFileHandler
            .handle(call)
            .await
            .unwrap()
            .code_mode_result(&payload);
        assert_eq!(result["results"][1]["text"], "before");
        // Search children already hydrated inline are not missing evidence.
        assert!(result.get("recovery").is_none());
        assert_eq!(result["results"][0]["value"]["total_matches"], 1);
        assert_eq!(
            result["results"][0]["value"]["hydrated_ranges"][0]["text"],
            "before\nneedle\nafter\n"
        );
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool expected")
        };
        jsonschema::validator_for(&spec.output_schema.as_ref().unwrap().to_value())
            .unwrap()
            .validate(&result)
            .unwrap();
    }

    #[tokio::test]
    async fn overlapping_search_context_preserves_file_coverage_in_both_consumers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlap.txt");
        let text = (0..7).map(|line| format!("candidate_{line} {}\r\n", "λ evidence; ".repeat(30))).collect::<String>();
        std::fs::write(&path, &text).unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function tool expected") };
        let validator = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            for (context_lines, expected_complete) in [(1, false), (3, true)] {
                let mut call = invocation(&path, json!([
                    {"kind":"search", "query":"candidate_2", "context_lines":context_lines},
                    {"kind":"search", "query":"candidate_4", "context_lines":context_lines}
                ]), false).await;
                if script {
                    call.source = ToolCallSource::CodeMode {
                        cell_id:"audit-overlap".into(), parent_call_id:None,
                        runtime_tool_call_id:"audit-overlap-read".into(),
                        nested_deadline:None, cancellation_cause:None,
                    };
                }
                let result = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
                assert_eq!(result["complete"], true);
                assert_eq!(result["file_complete"], expected_complete);
                assert!(result["results"][1]["value"]["hydrated_ranges"].as_array().unwrap().iter().any(|range| range["shared"] == true));
                validator.validate(&result).unwrap();
            }
        }
    }

    /// Builds a turn whose skills snapshot holds one real skill loaded from
    /// disk, plus the locator the catalog would advertise for it.
    async fn skill_invocation(
        root: &Path,
        locator_path: Option<String>,
        environment_id: Option<&str>,
    ) -> (ToolInvocation, String, std::path::PathBuf) {
        use codex_core_skills::SKILL_CATALOG_LOCATOR_PREFIX;
        use codex_core_skills::loader::SkillRoot;
        use codex_core_skills::loader::load_skills_from_roots;
        use codex_utils_absolute_path::test_support::PathExt;

        let skill_dir = root.join("demo-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: demo-skill\ndescription: demo skill for locator reads\n---\nfirst line\nsecond line\nthird line\n",
        )
        .unwrap();
        let outcome = load_skills_from_roots(
            vec![SkillRoot {
                path: root.abs(),
                scope: codex_protocol::protocol::SkillScope::Repo,
                file_system: Arc::clone(&codex_exec_server::LOCAL_FS),
                plugin_id: None,
                plugin_namespace: None,
                plugin_root: None,
            }],
            None,
        )
        .await;
        assert!(
            !outcome.skills.is_empty(),
            "fixture must load one skill: {:?}",
            outcome.errors
        );
        let locator = format!(
            "{SKILL_CATALOG_LOCATOR_PREFIX}{}",
            codex_core_skills::skill_catalog_id(&outcome.skills[0])
        );
        let snapshot = codex_core_skills::HostSkillsSnapshot::new(Arc::new(outcome));

        let (session, mut turn) = make_session_and_context().await;
        turn.permission_profile = PermissionProfile::Disabled;
        turn.turn_skills = crate::session::turn_context::TurnSkillsContext::new(snapshot);
        let mut arguments = json!({
            "path": locator_path.clone().unwrap_or_else(|| locator.clone()),
            "selectors": [{"kind": "lines", "start": 1, "end": 10}],
        });
        if let Some(environment_id) = environment_id {
            arguments["environment_id"] = json!(environment_id);
        }
        (
            ToolInvocation {
                session: Arc::new(session),
                step_context: StepContext::for_test(Arc::new(turn)),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "call-read-skill".to_string(),
                tool_name: ToolName::plain("read_file"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
            },
            locator,
            skill_md,
        )
    }

    /// The catalog advertises `skill:` locators, so a tool has to resolve them.
    /// Without this the model can see every skill listed and load none of them.
    #[tokio::test]
    async fn a_skill_locator_reads_its_skill_md_through_selectors() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog_call, locator, _) = skill_invocation(dir.path(), Some("skill:catalog".to_string()), None).await;
        let catalog_payload = catalog_call.payload.clone();
        let catalog = ReadFileHandler.handle(catalog_call).await.unwrap().code_mode_result(&catalog_payload);
        assert_eq!(catalog["path"], "skill:catalog");
        let entry: serde_json::Value = serde_json::from_str(catalog["results"][0]["text"].as_str().unwrap().trim()).unwrap();
        assert_eq!(entry["name"], "demo-skill");
        assert_eq!(entry["locator"], locator);
        assert_eq!(entry["description"], "demo skill for locator reads");
        assert!(entry.get("path").is_none());
        let (call, _locator, skill_md) = skill_invocation(dir.path(), None, None).await;
        let payload = call.payload.clone();
        let result = ReadFileHandler
            .handle(call)
            .await
            .unwrap()
            .code_mode_result(&payload);

        assert_eq!(result["results"][0]["status"], "ok");
        let text = result["results"][0]["text"].as_str().unwrap();
        assert!(text.contains("first line"), "{text}");
        assert!(text.contains("third line"), "{text}");
        assert_eq!(
            result["path"],
            skill_md.to_string_lossy().as_ref(),
            "the resolved skill path is reported, not the locator"
        );
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool expected")
        };
        jsonschema::validator_for(&spec.output_schema.as_ref().unwrap().to_value())
            .unwrap()
            .validate(&result)
            .unwrap();
    }

    #[tokio::test]
    async fn host_skill_reads_work_without_an_environment_but_ordinary_paths_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let (mut call, _, skill_md) = skill_invocation(dir.path(), None, None).await;
        Arc::get_mut(&mut call.step_context)
            .unwrap()
            .environments
            .turn_environments
            .clear();
        let payload = call.payload.clone();
        let result = ReadFileHandler
            .handle(call.clone())
            .await
            .unwrap()
            .code_mode_result(&payload);
        assert_eq!(result["results"][0]["status"], "ok");
        assert!(
            result["results"][0]["text"]
                .as_str()
                .unwrap()
                .contains("third line")
        );

        call.payload = ToolPayload::Function {
            arguments:
                json!({"path": skill_md, "selectors": [{"kind": "lines", "start": 1, "end": 10}]})
                    .to_string(),
        };
        let Err(FunctionCallError::RespondToModel(message)) = ReadFileHandler.handle(call).await
        else {
            panic!("ordinary paths must require an execution environment")
        };
        assert!(
            message.contains("read_file requires a selected execution environment"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn skill_locator_failures_name_what_went_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let (call, _, _) =
            skill_invocation(dir.path(), Some("skill:not-a-real-id".to_string()), None).await;
        let Err(FunctionCallError::RespondToModel(message)) = ReadFileHandler.handle(call).await
        else {
            panic!("expected an unknown locator to be rejected")
        };
        assert!(
            message.contains("skill:not-a-real-id") && message.contains("unknown skill locator"),
            "{message}"
        );

        let dir = tempfile::tempdir().unwrap();
        let (call, _, _) = skill_invocation(dir.path(), None, Some("some-other-environment")).await;
        let Err(FunctionCallError::RespondToModel(message)) = ReadFileHandler.handle(call).await
        else {
            panic!("expected an incompatible environment to be rejected")
        };
        assert!(
            message.contains("skill's own provider") && message.contains("some-other-environment"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn rejects_unsupported_input_and_preserves_filesystem_restrictions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.txt");
        std::fs::write(&path, "unchanged").unwrap();
        for (selectors, sandboxed, cancelled, expected) in [
            (json!([]), false, false, "requires 1-64 selectors"),
            (
                json!([{"kind": "lines", "start": 1, "end": 1}]),
                true,
                false,
                "sandboxed filesystem operations require configured runtime paths",
            ),
            (
                json!([{"kind": "lines", "start": 1, "end": 1}]),
                false,
                true,
                "read_file cancelled",
            ),
        ] {
            let call = invocation(&path, selectors, sandboxed).await;
            let artifact_dir = call.step_context.turn.config.codex_home.join("tool-output");
            if cancelled {
                call.cancellation_token.cancel();
            }
            let Err(FunctionCallError::RespondToModel(message)) =
                ReadFileHandler.handle(call).await
            else {
                panic!("expected rejected file read")
            };
            assert!(message.contains(expected), "{message}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "unchanged");
            assert!(
                !artifact_dir.exists(),
                "rejected reads must not retain snapshots"
            );
        }
        // Selector-local errors preserve successful siblings and the immutable
        // snapshot; they are not filesystem failures that abort the whole call.
        for selector in [
            json!({"kind":"json_pointer", "pointer":""}),
            json!({"kind":"section", "id":"unsupported"}),
        ] {
            let call = invocation(&path, json!([selector, {"kind":"lines", "start":1, "end":1}]), false).await;
            let result = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
            assert_eq!(result["complete"], false);
            assert!(result["artifact_id"].is_string());
            assert!(result["results"].as_array().unwrap().iter().any(|row| row["text"] == "unchanged"));
            if selector["kind"] == "json_pointer" {
                assert!(result["results"].as_array().unwrap().iter().any(|row| row["status"] == "invalid"));
            } else {
                assert_eq!(result["selector_errors"][0]["status"], "invalid_selector");
            }
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "unchanged");
        }
        let spec = serde_json::to_value(ReadFileHandler.spec()).unwrap();
        let validator = jsonschema::validator_for(&spec["parameters"]).unwrap();
        assert!(validator.is_valid(
            &json!({"path": "input.txt", "selectors": [{"kind": "section", "id": "x"}]})
        ));
        assert!(validator.is_valid(
            &json!({"path": "input.txt", "selectors": [{"kind": "lines", "start": 1, "end": 2}]})
        ));
    }

    #[tokio::test]
    async fn directory_error_includes_bounded_nonrecursive_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("immediate.txt"), "content").unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested").join("not-listed.txt"), "content").unwrap();
        let call = invocation(dir.path(), json!(null), false).await;
        let Err(FunctionCallError::RespondToModel(message)) = ReadFileHandler.handle(call).await else {
            panic!("directory is not file contents");
        };
        assert!(message.contains("immediate.txt"), "{message}");
        assert!(message.contains("nested"), "{message}");
        assert!(!message.contains("not-listed.txt"), "{message}");
        assert!(message.contains("\"complete\":true"), "{message}");
    }

    #[tokio::test]
    async fn file_and_recovery_selection_status_distinguish_failed_partial_and_complete_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selection.txt");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        for (selectors, expected) in [
            (json!([{"kind":"lines","start":0,"end":0}]), "failed"),
            (json!([{"kind":"lines","start":1,"end":1},{"kind":"lines","start":0,"end":0}]), "partial"),
            (json!([{"kind":"lines","start":1,"end":1}]), "complete"),
        ] {
            let mut call = invocation(&path, selectors.clone(), false).await;
            let output = ReadFileHandler.handle(call.clone()).await.unwrap();
            assert_eq!(output.success_for_logging(), expected == "complete");
            assert!(!output.code_mode_failure_is_error(), "selector diagnostics remain script data");
            let value = output.code_mode_result(&call.payload);
            assert_eq!(value["selection_status"], expected);
            assert_eq!(value["complete"], expected == "complete");
            assert_eq!(value["file_complete"], false);
            let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap().validate(&value).unwrap();
            if expected != "failed" { assert_eq!(value["results"][0]["text"], "first\n"); }
            call.tool_name = ToolName::plain("read_tool_output");
            call.payload = ToolPayload::Function { arguments:json!({
                "artifact_id":value["artifact_id"], "selectors":selectors,
            }).to_string() };
            let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap();
            assert_eq!(recovered.success_for_logging(), expected == "complete");
            assert!(!recovered.code_mode_failure_is_error(), "keep successful recovery siblings inspectable");
            let value = recovered.code_mode_result(&call.payload);
            assert_eq!(value["selection_status"], expected);
            assert_eq!(value["complete"], expected == "complete");
            let ToolSpec::Function(spec) = ReadToolOutputHandler.spec() else { panic!("function"); };
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap().validate(&value).unwrap();
            if expected != "failed" { assert_eq!(value["results"][0]["text"], "first\n"); }
        }
    }

    #[tokio::test]
    async fn rejects_binary_oversized_and_directory_inputs_without_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        let binary = dir.path().join("binary.dat");
        std::fs::write(&binary, [0xff, 0xfe]).unwrap();
        let oversized = dir.path().join("oversized.txt");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        for (path, expected) in [
            (missing.as_path(), "unable to locate"),
            (binary.as_path(), "requires UTF-8 text"),
            (oversized.as_path(), "exceeds the 8 MiB read limit"),
            (dir.path(), "requires a regular file"),
        ] {
            let call = invocation(
                path,
                json!([{"kind": "lines", "start": 1, "end": 1}]),
                false,
            )
            .await;
            let artifact_dir = call.step_context.turn.config.codex_home.join("tool-output");
            let Err(FunctionCallError::RespondToModel(message)) =
                ReadFileHandler.handle(call).await
            else {
                panic!("expected rejected file read")
            };
            assert!(message.contains(expected), "{message}");
            assert!(
                !artifact_dir.exists(),
                "rejected reads must not retain snapshots"
            );
        }
    }

    #[tokio::test]
    async fn verified10_bounded_enclosing_pages_survive_source_edits_and_recovery() {
        for script in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("items.rs");
            let first = "/// first\nfn first() {\n let needle = 1;\n}\n";
            let second = "/// second\nfn second() {\n let needle = 2;\n}\n";
            std::fs::write(&path, format!("{first}\n{second}")).unwrap();
            let mut call = invocation(&path, json!([{"kind":"search", "query":"needle", "max_results":1,
                "context_lines":0, "enclosing":true}]), false).await;
            if script { call.source = ToolCallSource::CodeMode { cell_id:"verified10".into(), parent_call_id:None,
                runtime_tool_call_id:"verified10-read".into(), nested_deadline:None, cancellation_cause:None }; }
            let result = ReadFileHandler.handle(call.clone()).await.unwrap();
            assert!(result.success_for_logging());
            let output = result.code_mode_result(&call.payload);
            assert_eq!(output["selection_status"], "complete");
            assert_eq!(output["results"][0]["value"]["matches_returned"], 1);
            assert_eq!(output["results"][0]["value"]["search_exhausted"], false);
            assert_eq!(output["results"][0]["value"]["hydrated_ranges"][0]["text"], first);
            assert_eq!(output["file_complete"], false);
            assert!(output.get("recovery").is_none(), "a delivered bounded page must not expand scope automatically");
            std::fs::write(&path, "fn replacement() {}\n").unwrap();
            call.tool_name = ToolName::plain("read_tool_output");
            call.payload = ToolPayload::Function { arguments:json!({"artifact_id":output["artifact_id"],
                "selectors":[output["results"][0]["continuation"]]}).to_string() };
            let ToolSpec::Function(spec) = ReadToolOutputHandler.spec() else { panic!("function"); };
            let ToolPayload::Function { arguments } = &call.payload else { unreachable!() };
            jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap()
                .validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap()).unwrap();
            let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap();
            assert!(recovered.success_for_logging());
            let recovered = recovered.code_mode_result(&call.payload);
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap().validate(&recovered).unwrap();
            assert_eq!(recovered["canonical_sha256"], output["canonical_sha256"]);
            assert_eq!(recovered["results"][0]["value"]["hydrated_ranges"][0]["text"], second);
            assert_eq!(recovered["results"][0]["value"]["enclosing_complete"], true);
            assert_eq!(recovered["results"][0]["value"]["search_exhausted"], true);
        }
    }

    #[tokio::test]
    async fn verified10_json_and_enrichment_failures_preserve_executable_siblings() {
        for source in ["not JSON\nneedle\n".to_string(), format!("{}0{}\nneedle\n", "[".repeat(3000), "]".repeat(3000))] {
            for script in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("raw.txt");
                std::fs::write(&path, &source).unwrap();
                let mut call = invocation(&path, json!([
                    {"kind":"lines", "start":2, "end":2},
                    {"kind":"json_pointer", "pointer":""},
                    {"kind":"search", "query":"needle", "enclosing":true, "context_lines":0}
                ]), false).await;
                if script { call.source = ToolCallSource::CodeMode { cell_id:"verified10-errors".into(), parent_call_id:None,
                    runtime_tool_call_id:"verified10-read".into(), nested_deadline:None, cancellation_cause:None }; }
                let result = ReadFileHandler.handle(call.clone()).await.unwrap();
                assert!(!result.success_for_logging());
                assert!(!result.code_mode_failure_is_error());
                let output = result.code_mode_result(&call.payload);
                assert_eq!(output["selection_status"], "partial");
                let results = output["results"].as_array().unwrap();
                assert!(results.iter().any(|result| result["text"] == "needle\n"));
                let json = results.iter().find(|result| result["selector"]["kind"] == "json_pointer").unwrap();
                assert_eq!(json["status"], "invalid");
                assert!(json["message"].as_str().unwrap().contains("valid JSON"));
                let search = results.iter().find(|result| result["selector"]["kind"] == "search").unwrap();
                assert_eq!(search["value"]["matches_returned"], 1);
                assert_ne!(search["value"]["enclosing_complete"], true);
                assert_eq!(output["selector_errors"][0]["selector_index"], 2);
                assert_eq!(output["selector_errors"][0]["complete"], false);
                let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
                jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap().validate(&output).unwrap();
            }
        }
    }

    #[test]
    fn verified10_recovery_recipe_batches_deduplicates_and_reports_omissions() {
        let results = (0..80).map(|i| json!({"selector":{"kind":"lines","start":i+1,"end":i+1},
            "status":"aggregate_omitted", "complete":false,
            "continuation":{"kind":"bytes","start":i*10,"end":i*10+5}})).collect::<Vec<_>>();
        let mut output = json!({"results":results});
        let duplicate = output["results"][0].clone();
        output["results"].as_array_mut().unwrap().push(duplicate);
        let recipe = file_recovery_recipe(&output, "snapshot").unwrap();
        assert!(recipe["arguments"].get("max_bytes").is_none(), "direct recipe cap changed");
        assert_eq!(recipe["arguments"]["selectors"].as_array().unwrap().len(), 64);
        assert_eq!(recipe["omitted_selectors"], 16);
        let recipe = file_recovery_recipe(&json!({"results":[
            {"status":"selector_too_large", "complete":false, "canonical_range":{"start":0,"end":100},
             "child_selectors":[{"kind":"bytes","start":0,"end":10}], "continuation":{"kind":"bytes","start":0,"end":10}},
            {"status":"aggregate_omitted", "complete":false, "continuation":{"kind":"lines","start":20,"end":30}}
        ]}), "snapshot").unwrap();
        assert_eq!(recipe["arguments"]["selectors"], json!([
            {"kind":"bytes","start":0,"end":10}, {"kind":"bytes","start":10,"end":100},
            {"kind":"lines","start":20,"end":30}
        ]));
        for max_results in [1, 2] {
            let recipe = file_recovery_recipe(&json!({"results":[{
                "selector":{"kind":"search", "query":"needle", "max_results":max_results},
                "status":"ok", "complete":false,
                "value":{"total_matches":20, "matches_returned":1, "hydrated_ranges":[]},
                "child_selectors":[{"kind":"lines", "start":1, "end":1}],
                "continuation":{"kind":"search", "query":"needle", "start_byte":6, "max_results":max_results}
            }]}), "snapshot").unwrap();
            let selectors = recipe["arguments"]["selectors"].as_array().unwrap();
            assert_eq!(selectors.len(), max_results);
            assert_eq!(selectors[0], json!({"kind":"lines", "start":1, "end":1}));
            if max_results == 2 { assert_eq!(selectors[1]["max_results"], 1); }
        }
    }

    #[tokio::test]
    #[ignore = "opt-in end-to-end enclosing-search timing; not a speedup assertion"]
    #[expect(clippy::print_stdout, reason = "report measured read/parse/select/persist/serialize wall time")]
    async fn verified10_multi_query_enclosing_end_to_end_benchmark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("benchmark.rs");
        let source = (0..2000).map(|i| format!("/// docs {i}\nfn item_{i}() {{\n let query_{} = {i};\n}}\n", i % 8)).collect::<String>();
        std::fs::write(&path, &source).unwrap();
        let selectors = (0..8).map(|i| json!({"kind":"search", "query":format!("query_{i}"),
            "enclosing":true, "max_results":2, "context_lines":0})).collect::<Vec<_>>();
        for script in [false, true] {
            let mut call = invocation(&path, json!(selectors), false).await;
            if script { call.source = ToolCallSource::CodeMode { cell_id:"verified10-bench".into(), parent_call_id:None,
                runtime_tool_call_id:"verified10-bench-read".into(), nested_deadline:None, cancellation_cause:None }; }
            let mut elapsed = Vec::new();
            let mut output_bytes = 0;
            for _ in 0..7 {
                let start = std::time::Instant::now();
                let result = ReadFileHandler.handle(call.clone()).await.unwrap();
                let output = result.code_mode_result(&call.payload);
                output_bytes = serde_json::to_vec(&output).unwrap().len();
                elapsed.push(start.elapsed().as_micros());
                assert!(result.success_for_logging());
                assert_eq!(output["results"].as_array().unwrap().len(), 8);
                for result in output["results"].as_array().unwrap() {
                    assert_eq!(result["value"]["matches_returned"], 2);
                    assert_eq!(result["value"]["enclosing_complete"], true);
                    assert_eq!(result["value"]["hydrated_ranges"].as_array().unwrap().len(), 2);
                }
            }
            let total: u128 = elapsed.iter().sum();
            elapsed.sort_unstable();
            println!("verified10_enclosing script={script} source_bytes={} queries=8 iterations=7 median_us={} total_us={total} output_bytes={output_bytes} model_requests=0 handler_calls=7 retries=0 recovery_calls=0", source.len(), elapsed[3]);
        }
    }
}
