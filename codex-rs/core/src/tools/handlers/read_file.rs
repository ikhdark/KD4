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
const DEFAULT_FILE_READ_TOKEN_CEILING: usize = 2_000;

#[cfg(test)]
thread_local! { static SOURCE_FILE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
#[path = "read_file_snapshot_recovery_tests.rs"]
mod snapshot_recovery_tests;


#[path = "code_sections.rs"]
pub(super) mod code_sections;
pub(crate) use code_sections::existing_tests_changed;

#[path = "read_file_structure.rs"]
mod structure;
use structure::FileSelector;

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
        Ok(Self {
            path: raw.path,
            environment_id: raw.environment_id,
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
        FunctionCallError::RespondToModel(message) => {
            FunctionCallError::RespondToModel(match accepted_selector_forms(arguments) {
                Some(forms) => format!("{message}. {forms}"),
                None => message,
            })
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
    // A live turn snapshot is never replayable under a filesystem identity.
    if args.path == crate::turn_diff_tracker::TURN_DIFF_LOCATOR {
        return None;
    }
    Some(json!({
        "path": args.path,
        "environment_id": args.environment_id,
        "selectors": args.selectors,
    }))
}


impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read_file")
    }

    fn spec(&self) -> ToolSpec {
        let mut selectors = JsonSchema::array(file_selector_schema(), Some("Omit to read the first page immediately. Batch known ranges here. Search results include text in results[].value.hydrated_ranges; enclosing:true also returns enclosing Rust/Python items as line selections. json_pointer selects a complete JSON value. {\"kind\":\"section\",\"id\":\"outline\"} lists items; symbol and enclosing share its parser; qualified names (Type::method or Class.method) disambiguate items. Oversized selections return child_selectors.".to_string()));
        selectors.min_items = Some(1);
        selectors.max_items = Some(READ_TOOL_OUTPUT_MAX_SELECTORS as u64);
        let mut output = read_tool_output_output_schema(file_selector_schema());
        output["properties"]["path"] = json!({"type": "string"});
        output["properties"]["environment_id"] = json!({"type":"string", "description":"Actual resolved execution environment, or host-skills/host-context for host-owned sources."});
        output["properties"]["canonical_uri"] = json!({"type":"string", "description":"Canonical path URI in the resolved environment, or the host-owned locator."});
        output["properties"]["total_lines"] = json!({"type": "integer", "minimum": 0});
        output["properties"]["source_sha256"] = json!({"type": "string", "description": "SHA-256 of the complete selected source representation, including bytes outside this page. Default attached HTML can be rendered text: use explicit selectors to obtain the original file hash and exact bytes before a revision-bound codex-range patch."});
        output["properties"]["artifact_id"] = json!({"type": ["string", "null"], "description": "Immutable snapshot identity when retained; omitted or null for ordinary complete inline reads or unavailable storage. An active typed inspect:path obligation retains its acquisition identity even for a complete inline selection."});
        output["properties"]["canonical_sha256"]["description"] = json!(
            "Compatibility alias for source_sha256; omitted from model presentation when identical."
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
                            | "canonical_sha256"
                            | "delivered_selection_complete"
                            | "retained_bytes"
                            | "retained_artifact_complete"
                    )
                )
            });
            required.push(json!("source_sha256"));
        }
        output["properties"]["retained_bytes"]["description"] = json!(
            "Retained source bytes; omitted in presentation only for inline-only reads with no artifact, zero retained bytes and no snapshot error."
        );
        output["properties"]["retained_artifact_complete"]["description"] = json!(
            "Whether all source bytes are retained; omitted alongside retained_bytes only for inline-only reads with no artifact (false)."
        );
        output["properties"]["file_complete"] = json!({"type": "boolean", "description": "The returned exact bytes and hydrated ranges cover the whole selected source representation in this tool result. This is acquisition coverage, not proof that enclosing script output displayed every byte or that the model semantically reviewed it. Retention, match coordinates, and recovery selectors alone do not establish coverage."});
        output["properties"]["selector_bindings"] = json!({"type":"array", "maxItems":64,
            "description":"Successful symbol/enclosing requests bound by their original zero-based selector index to exact snapshot ranges, before result sorting/merging. Resolution is not delivery: check results and completeness separately. Bound to source_sha256; no source is duplicated. qualified_name and kind are the bound item's parser-derived identity (owner included), not type resolution.",
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
                "source_sha256":{"type":"string"}, "omitted_candidates":{"type":"integer","minimum":0},
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
            description: format!("Read a UTF-8 file without shell quoting. The entire file must be UTF-8 and at most {MAX_FILE_MIB} MiB, even for a small byte selector. Structural selectors (symbol, enclosing, section outline/item IDs, and search with enclosing:true) support only Rust (.rs) and Python (.py). For PowerShell (.ps1) and other languages, use fixed-string search without enclosing or explicit line ranges; unsupported structural requests are rejected. Prefer explicit line ranges or supported symbols for known code; do not dump whole files to locate one function. Path alone returns a first page capped at about {DEFAULT_FILE_READ_TOKEN_CEILING} tokens plus continuation; small files fit inline. Host-owned skill/context guidance keeps its existing delivery budget. Default HTML outside the workspace may be rendered as text: hashes, ranges and recovery then describe that text; use explicit selectors for exact original markup. Use read_tool_output with the artifact_id and continuation only for needed remaining ranges of the immutable snapshot. complete describes the requested page or selectors; file_complete describes acquired source coverage, not final script display or semantic review. A complete inline read may have no retained artifact; that is not a read failure. Explicit lines, bytes and fixed-string search selectors remain exact; check each results[] status and recover only missing selections. In code mode explicit selections return up to 1 MiB to the script independently of its display budget: consume it there and print only needed evidence, not the entire recovered result. Each call reads current contents; retained snapshots are historical. Pass a skill: locator to read its SKILL.md; omit environment_id for host-owned skills."),
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
                ("offset".to_string(), JsonSchema { minimum: Some(1.into()), maximum: Some(usize::MAX.into()), ..JsonSchema::integer(Some("Legacy 1-based starting line; use instead of selectors, defaults to 1.".to_string())) }),
                ("limit".to_string(), JsonSchema { minimum: Some(1.into()), maximum: Some(usize::MAX.into()), ..JsonSchema::integer(Some("Legacy positive line count; defaults to 2000 when offset is supplied.".to_string())) }),
                ("environment_id".to_string(), JsonSchema::string(Some("Environment id; omit to use the primary environment.".to_string()))),
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
                        )
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
            let mut source_inspection = None;
            let mut inspection_proof_error = None;
            // The catalog advertises `skill:<id>` locators, so this tool has to
            // resolve them. Without it the model can see every skill listed and
            // load none of them.
            let (contents, resolved_path, environment_id, canonical_uri) = if args.path
                == crate::turn_diff_tracker::TURN_DIFF_LOCATOR
            {
                return Err(FunctionCallError::RespondToModel(
                    "context:turn-diff is not available; tracked deltas are not a reliable review source yet.".to_string(),
                ));
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
                read_environment_file(&invocation, &args, &mut source_inspection, &mut inspection_proof_error).await?
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
                    Some(policy.token_budget())
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
                    DEFAULT_FILE_READ_TOKEN_CEILING
                } else {
                    token_ceiling.min(DEFAULT_FILE_READ_TOKEN_CEILING)
                }
            } else {
                token_ceiling
            };
            let structure_path = args.path.clone();
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
            let mut file_complete = selector_errors.is_empty() && file_selection_complete(&result);
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
            output["file_complete"] = json!(file_complete);
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
                    file_complete = false;
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
    for selector in unique {
        let size = selector.to_string().len();
        if selectors.len() < READ_TOOL_OUTPUT_MAX_SELECTORS
            && bytes.saturating_add(size) <= 16 * 1024
        {
            bytes += size;
            selectors.push(selector);
        } else {
            omitted += 1;
        }
    }
    if selectors.is_empty() {
        return None;
    }
    let mut recipe = json!({"tool":"read_tool_output", "arguments":{"artifact_id":artifact_id, "selectors":selectors}});
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

/// Reads an ordinary path through the selected environment's filesystem.
#[cfg(test)]
async fn validate_retained_file_hash(
    step: &crate::session::step_context::StepContext,
    arguments: &str,
    expected_hash: &str,
    expected_bytes: u64,
) -> bool {
    if expected_bytes > MAX_FILE_BYTES as u64 { return false; }
    current_file_fingerprint(step, arguments, None).await
        .is_some_and(|(hash, bytes)| hash == expected_hash && bytes == expected_bytes)
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
    let Some(environment) = environment.filter(|environment| !environment.environment.is_remote())
    else {
        return None;
    };
    let Ok(path) = environment.cwd().join(&args.path) else {
        return None;
    };
    let sandbox = step
        .turn
        .file_system_sandbox_context(None, environment.cwd());
    #[cfg(test)]
    SOURCE_FILE_READS.with(|count| count.set(count.get() + 1));
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

/// Reads an ordinary path through the selected environment's filesystem.
async fn read_environment_file(
    invocation: &ToolInvocation,
    args: &ReadFileArgs,
    source_inspection: &mut Option<crate::tool_history::SourceInspectionOrigin>,
    inspection_proof_error: &mut Option<String>,
) -> Result<
    (
        String,
        String,
        String,
        String,
    ),
    FunctionCallError,
> {
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
    #[cfg(test)]
    SOURCE_FILE_READS.with(|count| count.set(count.get() + 1));
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
                            prune_hidden_directories: true,
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
                return Err(diagnostic_error(format!(
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
    Ok((
        contents,
        path.inferred_native_path_string(),
        environment.environment_id.clone(),
        path.to_string(),
    ))
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
    include!("read_file_inspection_tests.rs");

    #[test]
    fn read_schemas_reject_undecodable_offsets() {
        let ToolSpec::Function(read) = ReadFileHandler.spec() else {
            panic!("read_file tool");
        };
        let schema = serde_json::to_value(read.parameters).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for field in ["offset", "limit"] {
            for invalid in [json!(-1), json!(0), json!(1.5)] {
                let mut args = json!({"path":"file.rs"});
                args[field] = invalid;
                assert!(serde_json::from_value::<ReadFileArgs>(args.clone()).is_err());
                assert!(!validator.is_valid(&args), "{args}");
            }
            let mut args = json!({"path":"file.rs"});
            args[field] = json!(1);
            assert!(validator.is_valid(&args));
            assert!(serde_json::from_value::<ReadFileArgs>(args).is_ok());
        }
        let ToolSpec::Function(status) = ReadStatusHandler.spec() else {
            panic!("read_status tool");
        };
        let schema = serde_json::to_value(status.parameters).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for field in ["snapshot_offset", "range_offset", "artifact_offset"] {
            let query = json!({field:-1});
            assert!(
                serde_json::from_value::<crate::tool_history::ReadStatusQuery>(query.clone())
                    .is_err()
            );
            assert!(!validator.is_valid(&json!({"paths":["file.rs"],"query":query})));
            let query = json!({field:0});
            assert!(
                serde_json::from_value::<crate::tool_history::ReadStatusQuery>(query.clone())
                    .is_ok()
            );
            assert!(validator.is_valid(&json!({"paths":["file.rs"],"query":query})));
        }
    }

    #[tokio::test]
    async fn read_identity_and_json_pointer_survive_plain_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        std::fs::write(&path, r#"{"answer":42}"#).unwrap();
        let call = invocation(
            &path,
            json!([{"kind":"json_pointer","pointer":"/answer"}]),
            false,
        )
        .await;
        let output = ReadFileHandler
            .handle(call.clone())
            .await
            .unwrap()
            .code_mode_result(&call.payload);
        assert_eq!(
            output["environment_id"],
            call.step_context
                .environments
                .primary()
                .unwrap()
                .environment_id
        );
        assert_eq!(
            output["canonical_uri"],
            codex_utils_path_uri::PathUri::from_abs_path(
                &codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&path).unwrap()
            )
            .to_string()
        );
        let mut plain = call.clone();
        plain.payload = ToolPayload::Function { arguments: json!({"path":path,"force_fresh":true,"selectors":[{"kind":"bytes","start":0,"end":1}]}).to_string() };
        ReadFileHandler.handle(plain).await.unwrap();
        let (recovered, _) = read_tool_output_selectors_with_reuse(
            &call.step_context.turn.config.codex_home,
            &call.session.thread_id.to_string(),
            output["artifact_id"].as_str().unwrap(),
            vec![ToolOutputSelector::JsonPointer {
                pointer: "/answer".into(),
            }],
        )
        .await
        .unwrap();
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
                if cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied {
                    return;
                }
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
            let mut call =
                invocation(&path, json!([{"kind":"section","id":"outline"}]), false).await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "sections".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "sections-read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
            let output = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            assert_eq!(output["complete"], true);
            assert_eq!(output["file_complete"], false);
            let item = &output["results"][0]["value"]["details"]["items"][0];
            assert_eq!(item["name"], "works");
            assert_eq!(item["is_test"], true);
            let id = item["id"].as_str().unwrap().to_owned();
            let selector = ToolOutputSelector::Section { id: id.clone() };
            let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
                panic!("function tool");
            };
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value())
                .unwrap()
                .validate(&output)
                .unwrap();
            let mut selected_call = call.clone();
            selected_call.payload = ToolPayload::Function {
                arguments: json!({"path":path,"selectors":[selector]}).to_string(),
            };
            let selected = ReadFileHandler
                .handle(selected_call.clone())
                .await
                .unwrap()
                .code_mode_result(&selected_call.payload);
            assert_eq!(
                selected["results"][0]["text"],
                "#[test]\r\nfn works() { assert!(true); }"
            );
            // Later plain reads must not erase the section directory on a reused
            // hash-identical artifact, or invalidate already advertised handles.
            let mut plain_call = call.clone();
            plain_call.payload = ToolPayload::Function {
                arguments: json!({"path":path,"selectors":[{"kind":"lines","start":1,"end":1}]})
                    .to_string(),
            };
            let plain = ReadFileHandler
                .handle(plain_call.clone())
                .await
                .unwrap()
                .code_mode_result(&plain_call.payload);
            assert_ne!(plain["artifact_id"], output["artifact_id"]);
            std::fs::write(&path, "fn replacement() {}\n").unwrap();
            let (recovered, _) = read_tool_output_selectors_with_reuse(
                &call.step_context.turn.config.codex_home,
                &call.session.thread_id.to_string(),
                output["artifact_id"].as_str().unwrap(),
                vec![selector],
            )
            .await
            .unwrap();
            assert_eq!(
                recovered.results[0].text.as_deref(),
                Some("#[test]\r\nfn works() { assert!(true); }")
            );
            let changed = ReadFileHandler
                .handle(selected_call.clone())
                .await
                .unwrap()
                .code_mode_result(&selected_call.payload);
            assert_eq!(changed["results"][0]["status"], "not_found");
        }
    }

    #[tokio::test]
    async fn missing_file_suggests_existing_sibling_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("spec_plan.rs"), "private contents").unwrap();
        let call = invocation(&dir.path().join("spec.rs"), json!(null), false).await;
        let Err(FunctionCallError::RespondToModel(message)) = ReadFileHandler.handle(call).await
        else {
            panic!("missing file");
        };
        assert!(message.contains("spec_plan.rs"), "{message}");
        assert!(!message.contains("private contents"));
    }

    /// A recorded read guessed `core/src/tools/handlers/command_preflight.rs`
    /// after that module moved to another package, and the miss named nothing.
    #[tokio::test]
    async fn missing_file_names_its_same_name_file_moved_to_another_package() {
        let workspace = tempfile::tempdir().unwrap();
        let handlers = workspace.path().join("codex-rs/core/src/tools/handlers");
        let moved = workspace.path().join("codex-rs/command-analysis/src");
        std::fs::create_dir_all(&handlers).unwrap();
        std::fs::create_dir_all(&moved).unwrap();
        std::fs::write(
            handlers.join("mod.rs"),
            "pub(crate) use other::command_preflight;",
        )
        .unwrap();
        std::fs::write(moved.join("command_preflight.rs"), "private contents").unwrap();
        let mut call = invocation(&handlers.join("command_preflight.rs"), json!(null), false).await;
        Arc::get_mut(&mut call.step_context)
            .unwrap()
            .environments
            .turn_environments = vec![crate::session::turn_context::TurnEnvironment::new(
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            Arc::new(codex_exec_server::Environment::default_for_tests()),
            codex_utils_path_uri::PathUri::from_host_native_path(workspace.path()).unwrap(),
            None,
        )];
        let Err(FunctionCallError::RespondToModel(message)) = ReadFileHandler.handle(call).await
        else {
            panic!("missing file");
        };
        let suggestion = Path::new("command-analysis/src/command_preflight.rs");
        assert!(
            message.contains(
                &suggestion
                    .components()
                    .collect::<std::path::PathBuf>()
                    .display()
                    .to_string()
            ),
            "{message}"
        );
        assert!(!message.contains("private contents"));
    }

    /// Recorded attachments were 24-28% markup, and two had 23-26 physical
    /// lines (one 240,237 bytes long): first pages carried CSS and tags that
    /// every later request replayed, and line pages spanned whole reports.
    #[tokio::test]
    async fn attached_html_reads_as_text_while_selections_and_workspace_html_stay_exact() {
        use crate::session::turn_context::TurnEnvironment;
        use codex_utils_path_uri::PathUri;

        let html = concat!(
            "<!DOCTYPE html><html><head><title>Ignored</title><style>p{color:red}</style>",
            "<script>if (a < b) document.write('<p>no</p>');</script></head><body><main>",
            "<h1>KDA Audit &amp; plan</h1><p>First   finding\r\nspans <b>two</b> words.</p>",
            "<ul><li>one</li><li>two &lt;b&gt;</li></ul>",
            "<table><tr><th>ID</th><th>Status</th></tr><tr><td>F1</td><td>open</td></tr></table>",
            "<pre>\n  keep\n    spacing</pre><!-- hidden comment -->",
            "<p>See <a href=\"https://github.com/o/r/blob/0123abc/src/lib.rs#L4-L9\">lib.rs</a> and ",
            "<a href=\"https://github.com/o/r/blob/0123abc/src/a.rs#L1-L2\">src/a.rs:1-2</a> or ",
            "<a href=\"#local\">below</a>.</p><p>Caf&#233; &#x2713; &bogus; 1 &lt 2</p></main></body></html>",
        );
        let text = concat!(
            "# KDA Audit & plan\n\nFirst finding spans two words.\n\n- one\n- two <b>\n\n",
            "ID | Status\nF1 | open\n\n  keep\n    spacing\n\n",
            "See lib.rs [src/lib.rs:4-9] and src/a.rs:1-2 or below.\n\n",
            "Caf\u{e9} \u{2713} &bogus; 1 &lt 2\n",
        );
        let workspace = tempfile::tempdir().unwrap();
        let attachments = tempfile::tempdir().unwrap();
        let attached = attachments.path().join("Report.HTML");
        let local = workspace.path().join("page.html");
        std::fs::write(&attached, html).unwrap();
        std::fs::write(&local, html).unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool");
        };
        let schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        let read = async |path: &Path, selectors: serde_json::Value, script: bool| {
            let (session, mut turn) = make_session_and_context().await;
            turn.permission_profile = PermissionProfile::Disabled;
            turn.environments.turn_environments = vec![TurnEnvironment::new(
                codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
                Arc::new(codex_exec_server::Environment::default_for_tests()),
                PathUri::from_host_native_path(workspace.path()).unwrap(),
                None,
            )];
            let payload = ToolPayload::Function {
                arguments: json!({"path": path, "selectors": selectors}).to_string(),
            };
            let call = ToolInvocation {
                session: Arc::new(session),
                step_context: StepContext::for_test(Arc::new(turn)),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "call-read-html".to_string(),
                tool_name: ToolName::plain("read_file"),
                source: if script {
                    ToolCallSource::CodeMode {
                        cell_id: "html".into(),
                        parent_call_id: None,
                        runtime_tool_call_id: "html-read".into(),
                        nested_deadline: None,
                        cancellation_cause: None,
                    }
                } else {
                    ToolCallSource::Direct
                },
                payload: payload.clone(),
            };
            ReadFileHandler
                .handle(call)
                .await
                .unwrap()
                .code_mode_result(&payload)
        };
        for script in [false, true] {
            let output = read(&attached, json!(null), script).await;
            schema.validate(&output).unwrap();
            assert_eq!(output["results"][0]["text"], text, "script={script}");
            assert_eq!(output["file_complete"], true);
            assert_eq!(output["total_lines"], text.lines().count());
            assert_eq!(output["canonical_bytes"], text.len());
            // Codex-range patches bind this hash, so it names the delivered
            // text, never markup whose line numbers differ from it.
            assert_eq!(
                output["source_sha256"],
                crate::tool_history::sha256(text.as_bytes())
            );
            let message = output["results"][0]["message"].as_str().unwrap();
            assert!(
                message.contains(&format!("{} bytes of HTML", html.len())),
                "{message}"
            );
            let exact = read(
                &attached,
                json!([{"kind":"bytes","start":0,"end":15}]),
                script,
            )
            .await;
            assert_eq!(exact["results"][0]["text"], "<!DOCTYPE html>");
            let local = read(&local, json!(null), script).await;
            assert_eq!(local["results"][0]["text"], html);
            assert_eq!(
                local["source_sha256"],
                crate::tool_history::sha256(html.as_bytes())
            );
            assert!(local["results"][0].get("message").is_none());
        }
    }

    #[tokio::test]
    async fn structural_selector_failures_preserve_exact_siblings_and_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambiguous.rs");
        let source = "// λ\r\nimpl A { fn run() {} }\r\nimpl B { fn run() {} }\r\n";
        std::fs::write(&path, source).unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function");
        };
        let schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            let mut call = invocation(
                &path,
                json!([
                    {"kind":"lines","start":1,"end":1},
                    {"kind":"symbol","name":"run"},
                    {"kind":"symbol","name":"missing"},
                    {"kind":"symbol","name":"A::run"}
                ]),
                false,
            )
            .await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "mixed-selectors".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "mixed-read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
            let result = ReadFileHandler.handle(call.clone()).await.unwrap();
            assert!(!result.success_for_logging());
            assert!(!result.code_mode_failure_is_error());
            let output = result.code_mode_result(&call.payload);
            schema.validate(&output).unwrap();
            assert_eq!(output["selection_status"], "partial");
            assert_eq!(output["complete"], false);
            assert_eq!(output["file_complete"], false);
            assert!(output["results"].as_array().unwrap().iter().any(|result| {
                result["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("// λ\r\n"))
            }));
            assert_eq!(output["selector_errors"].as_array().unwrap().len(), 2);
            let error = &output["selector_errors"][0];
            assert_eq!(error["selector_index"], 1);
            assert_eq!(error["source_sha256"], output["source_sha256"]);
            assert_eq!(error["candidates"][0]["qualified_name"], "A::run");
            assert_eq!(
                error["candidates"][1]["selector"],
                json!({"kind":"lines","start":3,"end":3})
            );
            assert_eq!(error["omitted_candidates"], 0);
            assert_eq!(
                output["selector_bindings"],
                json!([{"selector_index":3,
                "qualified_name":"A::run", "kind":"fn",
                "resolved_selectors":[{"kind":"lines","start":2,"end":2}]}])
            );
        }
    }

    #[tokio::test]
    async fn oversized_structural_metadata_delivers_exact_sibling_and_retained_recovery() {
        use codex_protocol::openai_models::{TruncationMode, TruncationPolicyConfig};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many_owners.rs");
        let source = (0..8).map(|index| format!("impl Owner{index}{} {{ fn run() {{}} }}\n", "x".repeat(100))).collect::<String>();
        std::fs::write(&path, &source).unwrap();
        for (mode, limit) in [(TruncationMode::Bytes, 8_000), (TruncationMode::Tokens, 2_000)] {
            let mut selectors = vec![json!({"kind":"lines", "start":1, "end":1})];
            selectors.extend((0..63).map(|index| json!({"kind":"symbol", "name":format!("Missing{index}{}::run", "y".repeat(100))})));
            let (session, mut turn) = make_session_and_context().await;
            turn.permission_profile = PermissionProfile::Disabled;
            turn.model_info.truncation_policy = TruncationPolicyConfig { mode, limit };
            let call = ToolInvocation {
                session: Arc::new(session), step_context: StepContext::for_test(Arc::new(turn)),
                cancellation_token: CancellationToken::new(), tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "metadata-envelope".into(), tool_name: ToolName::plain("read_file"), source: ToolCallSource::Direct,
                payload: ToolPayload::Function { arguments: json!({"path":path, "selectors":selectors}).to_string() },
            };
            let result = ReadFileHandler.handle(call.clone()).await.unwrap();
            crate::tools::context::ensure_exact_model_delivery(result.as_ref(), &call).unwrap();
            let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = result.to_response_item(&call.call_id, &call.payload) else { panic!("function output") };
            let codex_protocol::models::FunctionCallOutputBody::Text(text) = output.body else { panic!("text output") };
            let delivered: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(delivered["complete"], false);
            assert_eq!(delivered["file_complete"], false);
            assert!(delivered["results"].as_array().unwrap().iter().any(|item| item["text"].as_str() == source.lines().next().map(|line| &source[..line.len() + 1])));
            let errors = delivered["selector_errors"].as_array().unwrap();
            assert_eq!(errors.len() as u64 + delivered["selector_errors_omitted"].as_u64().unwrap_or(0), 63);
            assert!(errors.iter().all(|error| error["status"] == "invalid_selector"));
            let artifact = delivered["artifact_id"].as_str().unwrap();
            let snapshot = crate::tools::command_output_artifact::load_tool_output_snapshot(
                &call.step_context.turn.config.codex_home, &call.session.thread_id.to_string(), artifact,
            ).await.unwrap();
            let recovered = snapshot.select(vec![ToolOutputSelector::Bytes { start:0, end:source.len() as u64 }], 8_000).await.unwrap();
            assert_eq!(recovered.results[0].text.as_deref(), Some(source.as_str()));
            assert_eq!(result.code_mode_result(&call.payload)["selector_errors"].as_array().unwrap().len(), 63);
            let mut projected = delivered.clone();
            add_file_projection_metadata(
                &mut projected, "criterion_evidence", &json!("x".repeat(20_000)), &call,
            );
            assert_eq!(projected, delivered, "late optional proof metadata must not displace exact bytes");
        }
    }

    #[tokio::test]
    async fn actionability_symbol_bindings_survive_merging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bindings.rs");
        std::fs::write(&path, "fn first() {}\nfn second() {}\n").unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function");
        };
        let schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            let mut call = invocation(
                &path,
                json!([
                    {"kind":"symbol","name":"second"}, {"kind":"symbol","name":"first"}
                ]),
                false,
            )
            .await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "bindings".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "bindings".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
            let output = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            schema.validate(&output).unwrap();
            assert_eq!(output["results"].as_array().unwrap().len(), 1);
            assert_eq!(
                output["selector_bindings"],
                json!([
                    {"selector_index":0,"qualified_name":"second","kind":"fn","resolved_selectors":[{"kind":"lines","start":2,"end":2}]},
                    {"selector_index":1,"qualified_name":"first","kind":"fn","resolved_selectors":[{"kind":"lines","start":1,"end":1}]}
                ])
            );
        }
    }

    #[tokio::test]
    async fn search_enclosing_and_python_symbols_share_exact_recoverable_items() {
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function");
        };
        let input_schema =
            jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap();
        let output_schema =
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for (file, unit, name) in [
            (
                "a.rs",
                "/// docs\n#[inline]\nfn target() {\n let needle = 1;\n let x = 2;\n let y = 3;\n let z = 4;\n let _ = needle + x + y + z;\n}\n",
                "target",
            ),
            (
                "a.py",
                "@decorator\ndef target():\n needle = 1\n x = 2\n y = 3\n z = 4\n return needle + x + y + z\n",
                "target",
            ),
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
                    call.source = ToolCallSource::CodeMode {
                        cell_id: "search-items".into(),
                        parent_call_id: None,
                        runtime_tool_call_id: "search-items-read".into(),
                        nested_deadline: None,
                        cancellation_cause: None,
                    };
                }
                let ToolPayload::Function { arguments } = &call.payload else {
                    unreachable!()
                };
                input_schema
                    .validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap())
                    .unwrap();
                let output = ReadFileHandler
                    .handle(call.clone())
                    .await
                    .unwrap()
                    .code_mode_result(&call.payload);
                output_schema.validate(&output).unwrap();
                let results = output["results"].as_array().unwrap();
                let search = results
                    .iter()
                    .find(|result| result["selector"]["kind"] == "search")
                    .unwrap();
                assert_eq!(search["value"]["hydrated_ranges"][0]["text"], unit);
                assert_eq!(search["value"]["enclosing_complete"], true);
                assert_eq!(search["value"]["matches_returned"], 1);
                assert_eq!(
                    search["value"]["matches"][0]["enclosing_item"]["qualified_name"],
                    name
                );
                assert_eq!(
                    search["value"]["matches"][0]["enclosing_item"]["start_line"],
                    1
                );
                assert_eq!(search["value"]["remaining_match_count"], 1);
                assert!(search["continuation"]["start_byte"].as_u64().unwrap() > 0);
                let outline = results
                    .iter()
                    .find(|result| result["selector"]["kind"] == "section")
                    .unwrap();
                assert_eq!(outline["value"]["details"]["items"][0]["start_line"], 1);
                let mut symbol_call = call.clone();
                symbol_call.payload = ToolPayload::Function {
                    arguments: json!({"path":path,"selectors":[{"kind":"symbol","name":name}]})
                        .to_string(),
                };
                let symbol = ReadFileHandler
                    .handle(symbol_call.clone())
                    .await
                    .unwrap()
                    .code_mode_result(&symbol_call.payload);
                assert_eq!(symbol["results"][0]["text"], unit);
                std::fs::write(&path, "replacement").unwrap();
                let (recovered, _) = read_tool_output_selectors_with_reuse(
                    &call.step_context.turn.config.codex_home,
                    &call.session.thread_id.to_string(),
                    output["artifact_id"].as_str().unwrap(),
                    vec![ToolOutputSelector::Lines {
                        start: 1,
                        end: unit.lines().count(),
                    }],
                )
                .await
                .unwrap();
                assert_eq!(recovered.results[0].text.as_deref(), Some(unit));
            }
        }
    }

    #[test]
    fn legacy_read_file_arguments_preserve_line_semantics() {
        let args: ReadFileArgs =
            parse_arguments(r#"{"file_path":"a.rs","offset":3,"limit":2}"#).unwrap();
        assert_eq!(args.path, "a.rs");
        assert_eq!(
            args.selectors,
            Some(vec![FileSelector::Exact(ToolOutputSelector::Lines {
                start: 3,
                end: 4
            })])
        );
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
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function");
        };
        let input_schema =
            jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap();
        let output_schema =
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            for selector in [
                json!({"kind":"symbol","name":"target"}),
                json!({"kind":"enclosing","line":4}),
            ] {
                std::fs::write(&path, &source).unwrap();
                let mut call = invocation(&path, json!([selector]), false).await;
                if script {
                    call.source = ToolCallSource::CodeMode {
                        cell_id: "rust-unit".into(),
                        parent_call_id: None,
                        runtime_tool_call_id: "rust-unit-read".into(),
                        nested_deadline: None,
                        cancellation_cause: None,
                    };
                }
                let ToolPayload::Function { arguments } = &call.payload else {
                    unreachable!()
                };
                input_schema
                    .validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap())
                    .unwrap();
                let result = ReadFileHandler
                    .handle(call.clone())
                    .await
                    .unwrap()
                    .code_mode_result(&call.payload);
                output_schema.validate(&result).unwrap();
                assert_eq!(result["results"][0]["text"], unit);
                assert_eq!(
                    result["results"][0]["selector"],
                    json!({"kind":"lines","start":2,"end":5})
                );
                assert_eq!(result["complete"], true);
                assert_eq!(result["file_complete"], false);
                std::fs::write(&path, "changed").unwrap();
                let (recovered, _) = read_tool_output_selectors_with_reuse(
                    &call.step_context.turn.config.codex_home,
                    &call.session.thread_id.to_string(),
                    result["artifact_id"].as_str().unwrap(),
                    vec![ToolOutputSelector::Lines { start: 2, end: 5 }],
                )
                .await
                .unwrap();
                assert_eq!(recovered.results[0].text.as_deref(), Some(unit));
            }
        }
    }

    #[tokio::test]
    async fn turn_diff_locator_is_not_exposed() {
        let call = invocation(
            Path::new(crate::turn_diff_tracker::TURN_DIFF_LOCATOR),
            json!([{"kind":"json_pointer","pointer":""}]),
            true,
        )
        .await;
        let ToolPayload::Function { arguments } = &call.payload else {
            unreachable!()
        };
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
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool");
        };
        let input_schema =
            jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap();
        let output_schema =
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            std::fs::write(&path, &source).unwrap();
            let mut call = invocation(&path, selectors.clone(), false).await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "json-unit".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "json-unit-read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
            let ToolPayload::Function { arguments } = &call.payload else {
                unreachable!()
            };
            input_schema
                .validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap())
                .unwrap();
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            output_schema.validate(&result).unwrap();
            assert_eq!(result["complete"], true);
            assert_eq!(result["file_complete"], false);
            assert_eq!(result["results"][0]["value"], expected);
            assert_eq!(
                result["source_sha256"],
                crate::tool_history::sha256(source.as_bytes())
            );
            let range = &result["results"][0]["canonical_range"];
            let start = range["start"].as_u64().unwrap();
            let end = range["end"].as_u64().unwrap();
            assert_eq!(&source[start as usize..end as usize], unit);

            // A fresh non-pointer selection of identical bytes must not erase
            // addresses already advertised by the earlier artifact.
            let mut plain_call = call.clone();
            plain_call.payload = ToolPayload::Function {
                arguments: json!({"path":path,"force_fresh":true,
                    "selectors":[{"kind":"lines","start":1,"end":1}]})
                .to_string(),
            };
            let plain = ReadFileHandler
                .handle(plain_call.clone())
                .await
                .unwrap()
                .code_mode_result(&plain_call.payload);
            assert_eq!(plain["complete"], true);
            assert!(plain["artifact_id"].is_string());

            // The known unit boundary and original formatting survive source edits.
            std::fs::write(&path, "replacement").unwrap();
            let (recovered, _) = read_tool_output_selectors_with_reuse(
                &call.step_context.turn.config.codex_home,
                &call.session.thread_id.to_string(),
                result["artifact_id"].as_str().unwrap(),
                vec![
                    ToolOutputSelector::JsonPointer {
                        pointer: "/a~1b/~0unit".into(),
                    },
                    ToolOutputSelector::Bytes { start, end },
                ],
            )
            .await
            .unwrap();
            assert!(recovered.complete);
            assert!(
                recovered
                    .results
                    .iter()
                    .any(|r| r.value.as_ref() == Some(&expected))
            );
            assert!(
                recovered
                    .results
                    .iter()
                    .any(|r| r.text.as_deref() == Some(unit))
            );
        }
    }

    #[tokio::test]
    async fn json_pointer_value_does_not_claim_exact_lexical_file_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lexical-spellings.json");
        // Escapes and whitespace parse away, so a decoded value is not lexical.
        let source = r#"{"policy":"allow", "escaped":"\u0061"}"#;
        std::fs::write(&path, source).unwrap();
        // A duplicated decoded member name has no unique value to present.
        let duplicates = dir.path().join("duplicate-keys.json");
        std::fs::write(&duplicates, r#"{"policy":"deny", "policy":"allow"}"#).unwrap();
        for script in [false, true] {
            for include_bytes in [false, true] {
                let mut selectors = json!([{"kind":"json_pointer", "pointer":""}]);
                if include_bytes {
                    selectors.as_array_mut().unwrap().push(json!({
                        "kind":"bytes", "start":0, "end":source.len(),
                    }));
                }
                let mut call = invocation(&path, selectors, false).await;
                if script {
                    call.source = ToolCallSource::CodeMode {
                        cell_id: "json-lexical".into(),
                        parent_call_id: None,
                        runtime_tool_call_id: "json-lexical-read".into(),
                        nested_deadline: None,
                        cancellation_cause: None,
                    };
                }
                let result = ReadFileHandler
                    .handle(call.clone())
                    .await
                    .unwrap()
                    .code_mode_result(&call.payload);
                assert_eq!(result["complete"], true);
                assert_eq!(
                    result["file_complete"], include_bytes,
                    "parsed values discard lexical spellings"
                );
                let selections = result["results"].as_array().unwrap();
                assert!(selections.iter().any(
                    |selection| selection["value"] == json!({"policy":"allow", "escaped":"a"})
                ));
                assert_eq!(
                    selections
                        .iter()
                        .any(|selection| selection["text"] == source),
                    include_bytes
                );
            }
            let mut call = invocation(
                &duplicates,
                json!([{"kind":"json_pointer", "pointer":""}]),
                false,
            )
            .await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "json-duplicates".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "json-duplicates-read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            assert_eq!(result["complete"], false, "{result}");
            assert_eq!(result["results"][0]["status"], "invalid", "{result}");
            assert!(result["results"][0]["value"].is_null(), "{result}");
        }
    }

    #[tokio::test]
    async fn json_payload_hydration_keys_are_not_lexical_acquisition_in_either_consumer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collision.json");
        let source = r#"{"picked":{"hydrated_ranges":[{"text":"x","canonical_range":{"start":0,"end":142}}]},"not_selected":"source bytes deliberately not requested"}"#;
        assert_eq!(source.len(), 142);
        std::fs::write(&path, source).unwrap();
        for script in [false, true] {
            let mut call = invocation(
                &path,
                json!([{"kind":"json_pointer","pointer":"/picked"}]),
                false,
            )
            .await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "collision".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "collision-read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            assert_eq!(result["complete"], true);
            assert_eq!(result["file_complete"], false);
            assert_eq!(
                result["results"][0]["value"],
                serde_json::from_str::<serde_json::Value>(source).unwrap()["picked"]
            );
        }
    }



    #[test]
    fn json_pointer_indexing_is_opt_in_and_missing_is_not_invalid() {
        let text = "not JSON\r\n";
        assert!(
            file_snapshot(text.into(), None)
                .unwrap()
                .json_pointers
                .is_empty()
        );
        let selectors = vec![ToolOutputSelector::JsonPointer { pointer: "".into() }];
        let invalid = file_snapshot(text.into(), Some(&selectors)).unwrap();
        let (result, _) =
            select_file_snapshot_for_script(&invalid, Some(selectors.clone())).unwrap();
        assert_eq!(
            result.results[0].status,
            crate::tools::command_output_artifact::ToolOutputSelectorStatus::Invalid
        );
        let canonical = file_snapshot("{\"value\": null}".into(), Some(&selectors)).unwrap();
        let selectors = vec![
            ToolOutputSelector::JsonPointer {
                pointer: "/missing".into(),
            },
            ToolOutputSelector::JsonPointer {
                pointer: "invalid".into(),
            },
            ToolOutputSelector::JsonPointer {
                pointer: "/value".into(),
            },
        ];
        let (result, _) = select_file_snapshot_for_script(&canonical, Some(selectors)).unwrap();
        use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
        for (pointer, status) in [
            ("/missing", ToolOutputSelectorStatus::NotFound),
            ("invalid", ToolOutputSelectorStatus::Invalid),
            ("/value", ToolOutputSelectorStatus::Ok),
        ] {
            let selected = result
                .results
                .iter()
                .find(|result| {
                    result.selector
                        == ToolOutputSelector::JsonPointer {
                            pointer: pointer.into(),
                        }
                })
                .unwrap();
            assert_eq!(selected.status, status);
            if pointer == "/value" {
                assert_eq!(selected.value, Some(json!(null)));
            }
        }
        assert!(!result.complete);
    }

    #[tokio::test]
    async fn oversized_script_selection_has_drainable_children() {
        let source = "requirement\n".repeat(110_000);
        let canonical = CanonicalToolResult::text(source.clone());
        let parent = ToolOutputSelector::Bytes {
            start: 0,
            end: canonical.exact_bytes,
        };
        let (result, _) =
            select_file_snapshot_for_script(&canonical, Some(vec![parent.clone()])).unwrap();
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
            let (page, _) =
                select_file_snapshot_for_script(&canonical, Some(vec![selector.clone()])).unwrap();
            let result = &page.results[0];
            if page.complete {
                assert_eq!(
                    result.canonical_range.unwrap().start,
                    recovered.len() as u64
                );
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
        let mut call = invocation(
            &path,
            json!([
                {"kind": "lines", "start": 1, "end": 1},
                {"kind": "lines", "start": 2, "end": 2},
                {"kind": "lines", "start": 3, "end": 3}
            ]),
            false,
        )
        .await;
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
        assert_eq!(
            result["results"][0]["selector"],
            json!({"kind":"lines", "start":1, "end":3})
        );
        assert_eq!(result["results"][0]["text"], line.repeat(3));
    }

    #[tokio::test]
    async fn selected_reads_have_stable_evidence_despite_random_artifact_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();
        let selectors = json!([{"kind":"lines","start":1,"end":1}]);
        let first = ReadFileHandler
            .handle(invocation(&path, selectors.clone(), false).await)
            .await
            .unwrap();
        let second = ReadFileHandler
            .handle(invocation(&path, selectors, false).await)
            .await
            .unwrap();
        assert!(first.sampling_request_signal().is_some());
        assert_eq!(
            first.sampling_request_signal().unwrap()["semantic_evidence"],
            second.sampling_request_signal().unwrap()["semantic_evidence"],
        );
        let other = ReadFileHandler
            .handle(invocation(&path, json!([{"kind":"lines","start":2,"end":2}]), false).await)
            .await
            .unwrap();
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
            }})
            .to_string()
            .into_bytes(),
        )
        .unwrap();
        let mut document = payload;
        document["evidence_lineage"] = stdout["evidence_lineage"].clone();
        let body = "Captured paths: a.rs, café.rs\n";
        let mut header = stdout;
        header["evidence_lineage"]["content_sha256"] =
            json!(crate::tool_history::sha256(body.as_bytes()));
        for report in [
            serde_json::to_string_pretty(&document).unwrap(),
            format!("<!-- codex-evidence: {header} -->\n{body}"),
        ] {
            std::fs::write(&path, &report).unwrap();
            let first = ReadFileHandler
                .handle(
                    invocation(
                        &path,
                        json!([{"kind":"lines", "start":1, "end":100}]),
                        false,
                    )
                    .await,
                )
                .await
                .unwrap();
            let evidence = first.sampling_request_signal().unwrap()["semantic_evidence"].clone();
            assert_eq!(evidence["source"], "read_file");
            assert_eq!(evidence["producer"], expected);
            let mut selections = Vec::new();
            for (start, end) in [(1, 1), (2, 2), (1, 2)] {
                let page = ReadFileHandler
                    .handle(
                        invocation(
                            &path,
                            json!([{"kind":"lines", "start":start, "end":end}]),
                            false,
                        )
                        .await,
                    )
                    .await
                    .unwrap();
                selections
                    .push(page.sampling_request_signal().unwrap()["semantic_evidence"].clone());
            }
            assert_ne!(selections[0], selections[1], "disjoint report pages");
            assert_ne!(selections[0], selections[2], "overlapping report pages");
            assert_ne!(
                selections[0], expected,
                "partial report is not whole producer evidence"
            );
            // A stale copied header must not hide an edit to the report.
            std::fs::write(&path, report.replace("a.rs", "b.rs")).unwrap();
            let changed = ReadFileHandler
                .handle(
                    invocation(
                        &path,
                        json!([{"kind":"lines", "start":1, "end":100}]),
                        false,
                    )
                    .await,
                )
                .await
                .unwrap();
            assert_eq!(
                changed.sampling_request_signal().unwrap()["semantic_evidence"]["source"],
                "read_file"
            );
            assert_ne!(
                changed.sampling_request_signal().unwrap()["semantic_evidence"],
                expected
            );
        }
        std::fs::write(
            &path,
            r#"{"evidence_lineage":{"source":"source_inventory","identity":"old"}}"#,
        )
        .unwrap();
        let unsigned = ReadFileHandler
            .handle(
                invocation(
                    &path,
                    json!([{"kind":"lines", "start":1, "end":100}]),
                    false,
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(
            unsigned.sampling_request_signal().unwrap()["semantic_evidence"]["source"],
            "read_file"
        );
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
            let output = ReadFileHandler
                .handle(
                    invocation(
                        &path,
                        json!([{"kind":"lines", "start":1, "end":100}]),
                        false,
                    )
                    .await,
                )
                .await
                .unwrap();
            identities.push(output.sampling_request_signal().unwrap()["semantic_evidence"].clone());
        }
        assert_eq!(identities[0]["producer"], identities[1]["producer"]);
        assert_ne!(identities[0]["identity"], identities[1]["identity"]);
    }

    #[tokio::test]
    async fn read_status_resolves_paths_in_the_selected_environment() {
        use crate::session::turn_context::TurnEnvironment;
        use codex_utils_path_uri::PathUri;

        // Select a second environment: with only the primary, ignoring the
        // requested id would resolve to the same path.
        let selected = tempfile::tempdir().unwrap();
        let (session, mut turn) = make_session_and_context().await;
        turn.permission_profile = PermissionProfile::Disabled;
        turn.environments
            .turn_environments
            .push(TurnEnvironment::new(
                "selected".into(),
                Arc::new(codex_exec_server::Environment::default_for_tests()),
                PathUri::from_host_native_path(selected.path()).unwrap(),
                None,
            ));
        let resolved = |environment: &TurnEnvironment| {
            environment
                .cwd()
                .join("source.rs")
                .unwrap()
                .inferred_native_path_string()
        };
        let primary = resolved(turn.environments.primary().unwrap());
        let expected = resolved(turn.environments.turn_environments.last().unwrap());
        assert_ne!(primary, expected);
        let payload = ToolPayload::Function {
            arguments: json!({"paths":["source.rs"], "environment_id":"selected"}).to_string(),
        };
        let call = ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::new(turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-read-status".to_string(),
            tool_name: ToolName::plain("read_status"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        };
        let output = ReadStatusHandler
            .handle(call)
            .await
            .unwrap()
            .code_mode_result(&payload);
        assert_eq!(output["paths"][0]["path"], expected);
        assert_eq!(output["paths"][0]["status"], "unknown");
        let ToolSpec::Function(spec) = ReadStatusHandler.spec() else {
            panic!("function tool");
        };
        let schema = serde_json::to_value(spec.parameters).unwrap();
        assert!(schema["properties"].get("environment_id").is_some());
    }

    pub(super) async fn invocation(
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
        let items = (0..20)
            .map(|index| {
                codex_protocol::protocol::RolloutItem::ResponseItem(
                    codex_protocol::models::ResponseItem::FunctionCall {
                        id: None,
                        call_id: format!("pending-{index:02}"),
                        name: "exec_command".into(),
                        namespace: None,
                        arguments: json!({"cmd":"SECRET_COMMAND"}).to_string(),
                        internal_chat_message_metadata_passthrough: None,
                    },
                )
            })
            .collect::<Vec<_>>();
        session.persist_rollout_items_durable(&items).await.unwrap();
        session
            .persist_rollout_items_durable(&[
                codex_protocol::protocol::RolloutItem::TurnContext(
                    call.step_context.turn.to_turn_context_item(),
                ),
                items[0].clone(),
            ])
            .await
            .unwrap();
        let before = tokio::fs::read(&path).await.unwrap();
        call.payload = ToolPayload::Function {
            arguments: json!({"path":"context:unsettled-tools/8"}).to_string(),
        };
        let output = ReadFileHandler
            .handle(call.clone())
            .await
            .unwrap()
            .code_mode_result(&call.payload);
        let page: serde_json::Value =
            serde_json::from_str(output["results"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(page["operations"][0]["call_id"], "pending-11");
        assert_eq!(page["unresolved_count"], 20);
        assert_eq!(page["recovery_path"], "context:unsettled-tools/16");
        assert_eq!(page["history_complete"], true);
        assert!(!output.to_string().contains("SECRET_COMMAND"));
        assert_eq!(tokio::fs::read(path).await.unwrap(), before);
        call.payload = ToolPayload::Function {
            arguments: json!({
                "path":"context:unsettled-tools/16", "environment_id":"local"
            })
            .to_string(),
        };
        assert!(ReadFileHandler.handle(call).await.is_err());
    }

    #[tokio::test]
    async fn desktop_locator_recovers_full_current_guidance_without_workspace_dependencies() {
        let mut hashes = Vec::new();
        for source in ["original", "updated"] {
            let mut call = invocation(
                Path::new(crate::context::desktop_instructions::LOCATOR),
                json!(null),
                true,
            )
            .await;
            let full = format!(
                "<app-context>\n# Codex desktop context\n### Automations\n{source}\n</app-context>"
            );
            Arc::get_mut(&mut Arc::make_mut(&mut call.step_context).turn)
                .expect("unique fixture turn")
                .developer_instructions =
                Some(format!("private outside\n{full}\nother instructions"));
            assert!(
                crate::tool_history::source_dependencies_for_tool_call(
                    "read_file",
                    &call.payload,
                    Path::new("."),
                )
                .is_empty()
            );
            let payload = call.payload.clone();
            let output = ReadFileHandler.handle(call).await.unwrap();
            let raw = output.code_mode_result(&payload);
            assert_eq!(raw["file_complete"], true);
            assert_eq!(raw["results"][0]["text"], full);
            assert_eq!(
                output.sampling_request_signal().unwrap()["semantic_evidence"]["scope"]["environment"],
                "host-context"
            );
            hashes.push(raw["source_sha256"].clone());
        }
        assert_ne!(hashes[0], hashes[1]);
        let mut missing = invocation(
            Path::new(crate::context::desktop_instructions::LOCATOR),
            json!(null),
            false,
        )
        .await;
        Arc::get_mut(&mut Arc::make_mut(&mut missing.step_context).turn)
            .expect("unique fixture turn")
            .developer_instructions = None;
        assert!(ReadFileHandler.handle(missing).await.is_err());
    }

    #[tokio::test]
    async fn host_guidance_locators_serve_the_details_requests_no_longer_carry() {
        use crate::tools::registry::ToolRegistry;
        use crate::tools::router::ToolRouter;

        let (session, mut turn) = make_session_and_context().await;
        let guidance = "<app-context>\n# Codex desktop context\n### Thread Coordination\nUse `read_file` and `list_threads`.\n</app-context>";
        turn.developer_instructions = Some(guidance.to_string());
        turn.multi_agent_version = codex_protocol::protocol::MultiAgentVersion::V2;
        Arc::make_mut(&mut turn.config)
            .multi_agent_v2
            .root_agent_usage_hint_text = Some("Full team guidance.".to_string());
        // Only read_file is registered, so the guidance's other tool is absent.
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([Arc::new(ReadFileHandler) as Arc<dyn CoreToolRuntime>]),
            Vec::new(),
        ));
        let step_context = StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(router);
        let session = Arc::new(session);
        let read = |path: &str| {
            let payload = ToolPayload::Function {
                arguments: json!({ "path": path }).to_string(),
            };
            let invocation = ToolInvocation {
                session: Arc::clone(&session),
                step_context: Arc::clone(&step_context),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: format!("read-{path}"),
                tool_name: ToolName::plain("read_file"),
                source: ToolCallSource::Direct,
                payload: payload.clone(),
            };
            async move {
                let output = ReadFileHandler.handle(invocation).await.unwrap();
                output.code_mode_result(&payload)["results"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            }
        };

        let desktop = read(crate::context::desktop_instructions::LOCATOR).await;
        assert!(desktop.starts_with(guidance));
        assert!(desktop.contains("absent from the current inventory"));
        assert!(desktop.contains("Thread Coordination: list_threads."));
        assert_eq!(
            read(crate::session::multi_agents::USAGE_HINT_LOCATOR).await,
            "Full team guidance."
        );
        for locator in [
            crate::context::desktop_instructions::LOCATOR,
            crate::session::multi_agents::USAGE_HINT_LOCATOR,
        ] {
            assert!(
                crate::tool_history::source_dependencies_for_tool_call(
                    "read_file",
                    &ToolPayload::Function {
                        arguments: json!({ "path": locator }).to_string(),
                    },
                    Path::new("."),
                )
                .is_empty(),
                "{locator} is host context, not a workspace read"
            );
        }
    }

    #[tokio::test]
    async fn model_projection_compacts_inline_and_retained_reads_without_changing_raw_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projection.txt");
        std::fs::write(&path, "first λ\nsecond 日本語\n").unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool");
        };
        let validator = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for selectors in [
            json!(null),
            json!([{"kind":"lines","start":1,"end":1}]),
            json!([{"kind":"search","query":"first","context_lines":0}]),
        ] {
            let call = invocation(&path, selectors.clone(), false).await;
            let payload = call.payload.clone();
            let output = ReadFileHandler.handle(call).await.unwrap();
            let raw = output.code_mode_result(&payload);
            let codex_protocol::models::ResponseInputItem::FunctionCallOutput {
                output: response,
                ..
            } = output.to_response_item("read", &payload)
            else {
                panic!("function output");
            };
            let codex_protocol::models::FunctionCallOutputBody::Text(text) = response.body else {
                panic!("text output");
            };
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
                    selected["child_selectors"] = json!(
                        selected["value"]["hydrated_ranges"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|range| range["selector"].clone())
                            .collect::<Vec<_>>()
                    );
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
    async fn path_only_reads_are_small_but_explicit_ranges_keep_their_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        let source = "// a source line with enough text to expose accidental whole-file reads\n"
            .repeat(1_000);
        std::fs::write(&path, &source).unwrap();
        for script in [false, true] {
            let mut call = invocation(&path, json!(null), false).await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "small-default-read".into(),
                    parent_call_id: Some("outer".into()),
                    runtime_tool_call_id: "read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            let delivered = result["results"][0]["text"].as_str().unwrap();
            assert!(!delivered.is_empty());
            assert!(delivered.len() < 8_000, "path-only reads must not dump a 60+ KB file");
            assert!(source.starts_with(delivered));
            assert_eq!(result["complete"], true);
            assert_eq!(result["file_complete"], false);
            assert_eq!(result["continuation"], json!({
                "kind":"bytes", "start":delivered.len(), "end":source.len()
            }));
            assert_eq!(result["retained_artifact_complete"], true);
            if script {
                call.payload = ToolPayload::Function {
                    arguments: json!({"path":path,
                        "selectors":[{"kind":"lines","start":1,"end":1_000}]}).to_string(),
                };
                let explicit = ReadFileHandler
                    .handle(call.clone())
                    .await
                    .unwrap()
                    .code_mode_result(&call.payload);
                assert_eq!(explicit["results"][0]["text"], source);
                assert_eq!(explicit["file_complete"], true);
            }
        }
    }

    #[tokio::test]
    async fn nested_file_pages_ignore_display_budget_and_recover_original_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounded.txt");
        let original = "λ exact immutable source\r\n".repeat(60_000);
        std::fs::write(&path, &original).unwrap();
        let mut call = invocation(&path, json!(null), false).await;
        let cell = codex_code_mode::CellId::new("bounded-file-read".into());
        call.session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell, "outer");
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
        assert!(
            codex_utils_string::approx_token_count(&serde_json::to_string(&unbounded).unwrap())
                > 4_000,
            "the old fixed-size page would exceed the owner budget"
        );
        let mut retained_page = None;
        for budget in [0, 512, 4_000, 10_000] {
            call.session
                .services
                .code_mode_service
                .record_output_budget(&cell, Some(budget));
            assert_eq!(
                call.session
                    .services
                    .code_mode_service
                    .output_budget(cell.as_str()),
                Some(budget)
            );
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
        call.session
            .services
            .code_mode_service
            .record_output_budget(&cell, Some(4_000));
        call.tool_name = ToolName::plain("read_tool_output");
        call.payload = ToolPayload::Function {
            arguments: json!({
                "artifact_id": retained["artifact_id"],
                "selectors": [selector],
            })
            .to_string(),
        };
        let recovered = ReadToolOutputHandler
            .handle(call.clone())
            .await
            .unwrap()
            .code_mode_result(&call.payload);
        assert_eq!(
            recovered["complete"],
            true,
            "requested={selector}; stop={}; results={:?}",
            recovered["continuation_stop"],
            recovered["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|result| (
                    &result["status"],
                    &result["canonical_range"],
                    &result["continuation"]
                ))
                .collect::<Vec<_>>()
        );
        let start = selector["start"].as_u64().unwrap() as usize;
        let end = selector["end"].as_u64().unwrap() as usize;
        assert_eq!(recovered["results"][0]["text"], &original[start..end]);
        assert!(codex_utils_string::approx_token_count(&recovered.to_string()) <= 4_000);

        // The ready-to-pass recipe preserves the script-sized budget. The
        // serialized envelope may still require multiple exact recovery pages.
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function");
        };
        jsonschema::validator_for(&spec.output_schema.unwrap().to_value())
            .unwrap()
            .validate(&retained)
            .unwrap();
        let mut recipe = retained["recovery"]["arguments"].clone();
        assert_eq!(recipe["max_bytes"], READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES);
        let start = retained["continuation"]["start"].as_u64().unwrap() as usize;
        let mut offset = start;
        let mut tail = String::new();
        let mut page_recipe = recipe.clone();
        for page_index in 0..64 {
            call.payload = ToolPayload::Function {
                arguments: page_recipe.to_string(),
            };
            let page = ReadToolOutputHandler
                .handle(call.clone())
                .await
                .unwrap()
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
                assert!(
                    offset - before > 16_384,
                    "recipe must retain the script-sized budget"
                );
            }
            if page["complete"] == true {
                assert_eq!(offset, original.len());
                break;
            }
            let stop = &page["continuation_stop"];
            assert_eq!(stop["reason"], "budget");
            assert_eq!(stop["resumable"], true);
            assert_eq!(
                stop["selector"],
                json!({"kind":"bytes", "start":offset, "end":original.len()})
            );
            page_recipe["selectors"] = json!([stop["selector"].clone()]);
        }
        assert_eq!(offset, original.len());
        assert_eq!(tail, original[start..]);

        // Same requested suffix, old recipe: another recovery is necessary.
        recipe.as_object_mut().unwrap().remove("max_bytes");
        call.payload = ToolPayload::Function {
            arguments: recipe.to_string(),
        };
        let paged = ReadToolOutputHandler
            .handle(call.clone())
            .await
            .unwrap()
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
            assert!(
                codex_utils_string::approx_token_count(&serde_json::to_string(&first).unwrap())
                    <= ceiling
            );
            for selector in crate::tools::command_output_artifact::bounded_page_selectors(
                &artifact_id,
                &canonical.bytes,
                0,
                canonical.exact_bytes,
                ceiling,
            ) {
                let selected = crate::tools::command_output_artifact::select_producer_snapshot(
                    &canonical,
                    &artifact_id,
                    vec![selector],
                    ceiling,
                )
                .unwrap();
                assert!(
                    selected.complete,
                    "every advertised page must fit: {selected:?}"
                );
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
            call.session
                .services
                .code_mode_service
                .record_cell_parent_call_id(&cell, "outer");
            call.source = ToolCallSource::CodeMode {
                cell_id: cell.to_string(),
                parent_call_id: Some("outer".into()),
                runtime_tool_call_id: "selected-read".into(),
                nested_deadline: None,
                cancellation_cause: None,
            };
            call.session
                .services
                .code_mode_service
                .record_output_budget(&cell, Some(4_000));
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
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
                assert_eq!(
                    first["child_selectors"][1],
                    json!({"kind":"bytes", "start":child_end, "end":end})
                );
            }
        }
    }

    #[tokio::test]
    async fn identical_files_keep_distinct_provenance_through_edit_and_restart() {
        use crate::tool_history::SourceDependencyV1;
        use crate::tool_history::ToolHistoryLoadOutcome;
        use crate::tool_history::WorkspaceEvidenceObservation;
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
        call_b.payload = ToolPayload::Function {
            arguments: json!({"path":b,
            "selectors":[{"kind":"lines", "start":1, "end":1}]})
            .to_string(),
        };
        let output_a = ReadFileHandler.handle(call_a.clone()).await.unwrap();
        let output_b = ReadFileHandler.handle(call_b.clone()).await.unwrap();
        let a_value = output_a.code_mode_result(&call_a.payload);
        let b_value = output_b.code_mode_result(&call_b.payload);
        assert_ne!(a_value["artifact_id"], b_value["artifact_id"]);
        let mut history = call_a
            .session
            .lock_history_state_for_test()
            .await
            .tool_history_state();
        for (call, output, path) in [(&call_a, &output_a, &a), (&call_b, &output_b, &b)] {
            let item = output.to_response_item(&call.call_id, &call.payload).into();
            history.register_workspace_evidence(
                WorkspaceEvidenceObservation::from_response_item(
                    None,
                    &item,
                    BTreeSet::from([SourceDependencyV1::new(path, false)]),
                )
                .unwrap(),
            );
        }
        let home = &call_a.step_context.turn.config.codex_home;
        let thread = call_a.session.thread_id.to_string();
        let id = a_value["artifact_id"].as_str().unwrap();
        for edited in [false, true] {
            if edited {
                std::fs::write(&a, "changed A only\n").unwrap();
                assert!(
                    history
                        .invalidate_source_dependencies(Some(&BTreeSet::from([a.clone()])), None)
                );
            }
            for restarted in [false, true] {
                if restarted {
                    crate::tool_history::persist_tool_history_state(home, &thread, &history)
                        .await
                        .unwrap();
                    let ToolHistoryLoadOutcome::Loaded(restored) =
                        crate::tool_history::load_tool_history_state(home, &thread).await
                    else {
                        panic!("history restart")
                    };
                    history = restored;
                }
                let provenance = serde_json::to_value(&history).unwrap();
                assert_eq!(provenance["internal_artifact_origins"][id][0], "file-a");
                assert_eq!(
                    provenance["workspace_evidence"]["file-a"]["source_dependencies_current"],
                    !edited
                );
                assert_eq!(
                    provenance["workspace_evidence"]["file-b"]["source_dependencies_current"],
                    true
                );
                let (recovered, _) = read_tool_output_selectors_with_reuse(
                    home,
                    &thread,
                    id,
                    vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
                )
                .await
                .unwrap();
                assert_eq!(
                    recovered.results[0].text.as_deref(),
                    Some("identical source\n")
                );
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
        assert_eq!(
            result["recovery"],
            json!({
                "tool": "read_tool_output",
                "arguments": {"artifact_id": artifact_id, "selectors": [selector.clone()]}
            })
        );
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
            recovery_arguments =
                json!({"artifact_id": artifact_id, "selectors": [selector.clone()]});
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
        // The fixture session owns its temporary home and the snapshot in it.
        let _session = Arc::clone(&call.session);
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
        let text = (0..7)
            .map(|line| format!("candidate_{line} {}\r\n", "λ evidence; ".repeat(30)))
            .collect::<String>();
        std::fs::write(&path, &text).unwrap();
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("function tool expected")
        };
        let validator = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
        for script in [false, true] {
            for (context_lines, expected_complete) in [(1, false), (3, true)] {
                let mut call = invocation(
                    &path,
                    json!([
                        {"kind":"search", "query":"candidate_2", "context_lines":context_lines},
                        {"kind":"search", "query":"candidate_4", "context_lines":context_lines}
                    ]),
                    false,
                )
                .await;
                if script {
                    call.source = ToolCallSource::CodeMode {
                        cell_id: "audit-overlap".into(),
                        parent_call_id: None,
                        runtime_tool_call_id: "audit-overlap-read".into(),
                        nested_deadline: None,
                        cancellation_cause: None,
                    };
                }
                let result = ReadFileHandler
                    .handle(call.clone())
                    .await
                    .unwrap()
                    .code_mode_result(&call.payload);
                assert_eq!(result["complete"], true);
                assert_eq!(result["file_complete"], expected_complete);
                assert!(
                    result["results"][1]["value"]["hydrated_ranges"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|range| range["shared"] == true)
                );
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
        let (catalog_call, locator, _) =
            skill_invocation(dir.path(), Some("skill:catalog".to_string()), None).await;
        let catalog_payload = catalog_call.payload.clone();
        let catalog = ReadFileHandler
            .handle(catalog_call)
            .await
            .unwrap()
            .code_mode_result(&catalog_payload);
        assert_eq!(catalog["path"], "skill:catalog");
        let entry: serde_json::Value =
            serde_json::from_str(catalog["results"][0]["text"].as_str().unwrap().trim()).unwrap();
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
        assert_eq!(
            text,
            "---\nname: demo-skill\ndescription: demo skill for locator reads\n---\nfirst line\nsecond line\nthird line\n"
        );
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
        assert_eq!(
            result["results"][0]["text"],
            "---\nname: demo-skill\ndescription: demo skill for locator reads\n---\nfirst line\nsecond line\nthird line\n"
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
            let call = invocation(
                &path,
                json!([selector, {"kind":"lines", "start":1, "end":1}]),
                false,
            )
            .await;
            let result = ReadFileHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            assert_eq!(result["complete"], false);
            assert!(result["artifact_id"].is_string());
            assert!(
                result["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["text"] == "unchanged")
            );
            if selector["kind"] == "json_pointer" {
                assert!(
                    result["results"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| row["status"] == "invalid")
                );
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
        let Err(FunctionCallError::RespondToModel(message)) = ReadFileHandler.handle(call).await
        else {
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
            (
                json!([{"kind":"lines","start":1,"end":1},{"kind":"lines","start":0,"end":0}]),
                "partial",
            ),
            (json!([{"kind":"lines","start":1,"end":1}]), "complete"),
        ] {
            let mut call = invocation(&path, selectors.clone(), false).await;
            let output = ReadFileHandler.handle(call.clone()).await.unwrap();
            assert_eq!(output.success_for_logging(), expected == "complete");
            assert!(
                !output.code_mode_failure_is_error(),
                "selector diagnostics remain script data"
            );
            let value = output.code_mode_result(&call.payload);
            assert_eq!(value["selection_status"], expected);
            assert_eq!(value["complete"], expected == "complete");
            assert_eq!(value["file_complete"], false);
            let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
                panic!("function");
            };
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value())
                .unwrap()
                .validate(&value)
                .unwrap();
            if expected != "failed" {
                assert_eq!(value["results"][0]["text"], "first\n");
            }
            call.tool_name = ToolName::plain("read_tool_output");
            call.payload = ToolPayload::Function {
                arguments: json!({
                    "artifact_id":value["artifact_id"], "selectors":selectors,
                })
                .to_string(),
            };
            let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap();
            assert_eq!(recovered.success_for_logging(), expected == "complete");
            assert!(
                !recovered.code_mode_failure_is_error(),
                "keep successful recovery siblings inspectable"
            );
            let value = recovered.code_mode_result(&call.payload);
            assert_eq!(value["selection_status"], expected);
            assert_eq!(value["complete"], expected == "complete");
            let ToolSpec::Function(spec) = ReadToolOutputHandler.spec() else {
                panic!("function");
            };
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value())
                .unwrap()
                .validate(&value)
                .unwrap();
            if expected != "failed" {
                assert_eq!(value["results"][0]["text"], "first\n");
            }
        }
    }

    #[tokio::test]
    async fn read_failure_survives_secondary_filesystem_diagnostics() {
        use codex_exec_server::CopyOptions;
        use codex_exec_server::CreateDirectoryOptions;
        use codex_exec_server::ExecutorFileSystem;
        use codex_exec_server::ExecutorFileSystemFuture;
        use codex_exec_server::FileMetadata;
        use codex_exec_server::FileSystemReadStream;
        use codex_exec_server::FileSystemSandboxContext;
        use codex_exec_server::ReadDirectoryEntry;
        use codex_exec_server::RemoveOptions;
        use codex_exec_server::WalkOptions;
        use codex_exec_server::WalkOutcome;
        use codex_utils_path_uri::PathUri;
        use std::io;

        struct DiagnosticFileSystem {
            metadata: Result<FileMetadata, io::ErrorKind>,
            calls: std::sync::Mutex<Vec<&'static str>>,
        }
        impl ExecutorFileSystem for DiagnosticFileSystem {
            fn read_file_bounded<'a>(
                &'a self,
                _: &'a PathUri,
                _: usize,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
                Box::pin(async move {
                    self.calls.lock().unwrap().push("read");
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "primary-read-denied",
                    ))
                })
            }
            fn get_metadata<'a>(
                &'a self,
                _: &'a PathUri,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
                Box::pin(async move {
                    self.calls.lock().unwrap().push("metadata");
                    self.metadata
                        .clone()
                        .map_err(|kind| io::Error::new(kind, "secondary-metadata-failed"))
                })
            }
            fn walk<'a>(
                &'a self,
                _: &'a PathUri,
                _: WalkOptions,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
                Box::pin(async move {
                    self.calls.lock().unwrap().push("walk");
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "secondary-listing-failed",
                    ))
                })
            }
            fn canonicalize<'a>(
                &'a self,
                _: &'a PathUri,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, PathUri> {
                panic!("unexpected canonicalize")
            }
            fn read_file<'a>(
                &'a self,
                _: &'a PathUri,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
                panic!("unexpected unbounded read")
            }
            fn read_file_stream<'a>(
                &'a self,
                _: &'a PathUri,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
                panic!("unexpected stream")
            }
            fn write_file<'a>(
                &'a self,
                _: &'a PathUri,
                _: Vec<u8>,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, ()> {
                panic!("unexpected write")
            }
            fn create_directory<'a>(
                &'a self,
                _: &'a PathUri,
                _: CreateDirectoryOptions,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, ()> {
                panic!("unexpected mkdir")
            }
            fn read_directory<'a>(
                &'a self,
                _: &'a PathUri,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
                panic!("unexpected directory read")
            }
            fn remove<'a>(
                &'a self,
                _: &'a PathUri,
                _: RemoveOptions,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, ()> {
                panic!("unexpected remove")
            }
            fn copy<'a>(
                &'a self,
                _: &'a PathUri,
                _: &'a PathUri,
                _: CopyOptions,
                _: Option<&'a FileSystemSandboxContext>,
            ) -> ExecutorFileSystemFuture<'a, ()> {
                panic!("unexpected copy")
            }
        }
        let regular = FileMetadata {
            is_file: true,
            is_directory: false,
            is_symlink: false,
            size: 1,
            created_at_ms: 0,
            modified_at_ms: 0,
        };
        for metadata in [
            Err(io::ErrorKind::NotFound),
            Err(io::ErrorKind::BrokenPipe),
            Ok(FileMetadata {
                size: MAX_FILE_BYTES as u64 + 1,
                ..regular.clone()
            }),
            Ok(FileMetadata {
                is_file: false,
                ..regular.clone()
            }),
            Ok(FileMetadata {
                is_file: false,
                is_directory: true,
                ..regular.clone()
            }),
            Ok(regular),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let file_system = Arc::new(DiagnosticFileSystem {
                metadata: metadata.clone(),
                calls: Default::default(),
            });
            let (session, mut turn) = make_session_and_context().await;
            turn.permission_profile = PermissionProfile::Disabled;
            turn.environments.turn_environments =
                vec![crate::session::turn_context::TurnEnvironment::new(
                    codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
                    Arc::new(
                        codex_exec_server::Environment::default_for_tests_with_filesystem(
                            file_system.clone(),
                        ),
                    ),
                    PathUri::from_host_native_path(directory.path()).unwrap(),
                    None,
                )];
            let call = ToolInvocation {
                session: Arc::new(session),
                step_context: StepContext::for_test(Arc::new(turn)),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "read-error-provenance".into(),
                tool_name: ToolName::plain("read_file"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: json!({"path":"source.rs"}).to_string(),
                },
            };
            let Err(FunctionCallError::RespondToModel(message)) =
                ReadFileHandler.handle(call).await
            else {
                panic!("failed read must not return source evidence");
            };
            // The operation's error is authoritative. A later observation can
            // explain it, but cannot replace it with a different failure/cause.
            assert!(
                message.contains("primary-read-denied"),
                "metadata={metadata:?}: {message}"
            );
            if metadata.is_err() {
                assert!(message.contains("secondary-metadata-failed"), "{message}");
            }
            let directory_metadata = metadata
                .as_ref()
                .is_ok_and(|metadata| metadata.is_directory);
            if directory_metadata {
                assert!(message.contains("secondary-listing-failed"), "{message}");
            }
            assert_eq!(
                *file_system.calls.lock().unwrap(),
                if directory_metadata {
                    vec!["read", "metadata", "walk"]
                } else {
                    vec!["read", "metadata"]
                }
            );
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
            let mut call = invocation(
                &path,
                json!([{"kind":"search", "query":"needle", "max_results":1,
                "context_lines":0, "enclosing":true}]),
                false,
            )
            .await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "verified10".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "verified10-read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
            let result = ReadFileHandler.handle(call.clone()).await.unwrap();
            assert!(result.success_for_logging());
            let output = result.code_mode_result(&call.payload);
            assert_eq!(output["selection_status"], "complete");
            assert_eq!(output["results"][0]["value"]["matches_returned"], 1);
            assert_eq!(output["results"][0]["value"]["search_exhausted"], false);
            assert_eq!(
                output["results"][0]["value"]["hydrated_ranges"][0]["text"],
                first
            );
            assert_eq!(output["file_complete"], false);
            assert!(
                output.get("recovery").is_none(),
                "a delivered bounded page must not expand scope automatically"
            );
            std::fs::write(&path, "fn replacement() {}\n").unwrap();
            call.tool_name = ToolName::plain("read_tool_output");
            call.payload = ToolPayload::Function {
                arguments: json!({"artifact_id":output["artifact_id"],
                "selectors":[output["results"][0]["continuation"]]})
                .to_string(),
            };
            let ToolSpec::Function(spec) = ReadToolOutputHandler.spec() else {
                panic!("function");
            };
            let ToolPayload::Function { arguments } = &call.payload else {
                unreachable!()
            };
            jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap())
                .unwrap()
                .validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap())
                .unwrap();
            let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap();
            assert!(recovered.success_for_logging());
            let recovered = recovered.code_mode_result(&call.payload);
            jsonschema::validator_for(&spec.output_schema.unwrap().to_value())
                .unwrap()
                .validate(&recovered)
                .unwrap();
            assert_eq!(recovered["canonical_sha256"], output["canonical_sha256"]);
            assert_eq!(
                recovered["results"][0]["value"]["hydrated_ranges"][0]["text"],
                second
            );
            assert_eq!(recovered["results"][0]["value"]["enclosing_complete"], true);
            assert_eq!(recovered["results"][0]["value"]["search_exhausted"], true);
        }
    }

    #[tokio::test]
    async fn verified10_json_and_enrichment_failures_preserve_executable_siblings() {
        for source in [
            "not JSON\nneedle\n".to_string(),
            format!("{}0{}\nneedle\n", "[".repeat(3000), "]".repeat(3000)),
        ] {
            for script in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("raw.txt");
                std::fs::write(&path, &source).unwrap();
                let mut call = invocation(
                    &path,
                    json!([
                        {"kind":"lines", "start":2, "end":2},
                        {"kind":"json_pointer", "pointer":""},
                        {"kind":"search", "query":"needle", "enclosing":true, "context_lines":0}
                    ]),
                    false,
                )
                .await;
                if script {
                    call.source = ToolCallSource::CodeMode {
                        cell_id: "verified10-errors".into(),
                        parent_call_id: None,
                        runtime_tool_call_id: "verified10-read".into(),
                        nested_deadline: None,
                        cancellation_cause: None,
                    };
                }
                let result = ReadFileHandler.handle(call.clone()).await.unwrap();
                assert!(!result.success_for_logging());
                assert!(!result.code_mode_failure_is_error());
                let output = result.code_mode_result(&call.payload);
                assert_eq!(output["selection_status"], "partial");
                let results = output["results"].as_array().unwrap();
                assert!(results.iter().any(|result| result["text"] == "needle\n"));
                let json = results
                    .iter()
                    .find(|result| result["selector"]["kind"] == "json_pointer")
                    .unwrap();
                assert_eq!(json["status"], "invalid");
                // Pointer validation reports both malformed and over-deep input
                // before selection, and directs recovery to the retained bytes.
                assert!(
                    json["message"]
                        .as_str()
                        .unwrap()
                        .contains("Malformed or depth-limited JSON"),
                    "{json}"
                );
                let search = results
                    .iter()
                    .find(|result| result["selector"]["kind"] == "search")
                    .unwrap();
                assert_eq!(search["value"]["matches_returned"], 1);
                assert_ne!(search["value"]["enclosing_complete"], true);
                assert_eq!(output["selector_errors"][0]["selector_index"], 2);
                assert_eq!(output["selector_errors"][0]["complete"], false);
                let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
                    panic!("function");
                };
                jsonschema::validator_for(&spec.output_schema.unwrap().to_value())
                    .unwrap()
                    .validate(&output)
                    .unwrap();
            }
        }
    }

    #[test]
    fn verified10_recovery_recipe_batches_deduplicates_and_reports_omissions() {
        let results = (0..80)
            .map(|i| {
                json!({"selector":{"kind":"lines","start":i+1,"end":i+1},
            "status":"aggregate_omitted", "complete":false,
            "continuation":{"kind":"bytes","start":i*10,"end":i*10+5}})
            })
            .collect::<Vec<_>>();
        let mut output = json!({"results":results});
        let duplicate = output["results"][0].clone();
        output["results"].as_array_mut().unwrap().push(duplicate);
        let recipe = file_recovery_recipe(&output, "snapshot").unwrap();
        assert!(
            recipe["arguments"].get("max_bytes").is_none(),
            "direct recipe cap changed"
        );
        assert_eq!(
            recipe["arguments"]["selectors"].as_array().unwrap().len(),
            64
        );
        assert_eq!(recipe["omitted_selectors"], 16);
        let recipe = file_recovery_recipe(&json!({"results":[
            {"status":"selector_too_large", "complete":false, "canonical_range":{"start":0,"end":100},
             "child_selectors":[{"kind":"bytes","start":0,"end":10}], "continuation":{"kind":"bytes","start":0,"end":10}},
            {"status":"aggregate_omitted", "complete":false, "continuation":{"kind":"lines","start":20,"end":30}}
        ]}), "snapshot").unwrap();
        assert_eq!(
            recipe["arguments"]["selectors"],
            json!([
                {"kind":"bytes","start":0,"end":10}, {"kind":"bytes","start":10,"end":100},
                {"kind":"lines","start":20,"end":30}
            ])
        );
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
            if max_results == 2 {
                assert_eq!(selectors[1]["max_results"], 1);
            }
        }
    }

    #[tokio::test]
    #[ignore = "opt-in end-to-end enclosing-search timing; not a speedup assertion"]
    #[expect(
        clippy::print_stdout,
        reason = "report measured read/parse/select/persist/serialize wall time"
    )]
    async fn verified10_multi_query_enclosing_end_to_end_benchmark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("benchmark.rs");
        let source = (0..2000)
            .map(|i| {
                format!(
                    "/// docs {i}\nfn item_{i}() {{\n let query_{} = {i};\n}}\n",
                    i % 8
                )
            })
            .collect::<String>();
        std::fs::write(&path, &source).unwrap();
        let selectors = (0..8)
            .map(|i| {
                json!({"kind":"search", "query":format!("query_{i}"),
            "enclosing":true, "max_results":2, "context_lines":0})
            })
            .collect::<Vec<_>>();
        for script in [false, true] {
            let mut call = invocation(&path, json!(selectors), false).await;
            if script {
                call.source = ToolCallSource::CodeMode {
                    cell_id: "verified10-bench".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "verified10-bench-read".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                };
            }
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
                    assert_eq!(
                        result["value"]["hydrated_ranges"].as_array().unwrap().len(),
                        2
                    );
                }
            }
            let total: u128 = elapsed.iter().sum();
            elapsed.sort_unstable();
            println!(
                "verified10_enclosing script={script} source_bytes={} queries=8 iterations=7 median_us={} total_us={total} output_bytes={output_bytes} model_requests=0 handler_calls=7 retries=0 recovery_calls=0",
                source.len(),
                elapsed[3]
            );
        }
    }
}
