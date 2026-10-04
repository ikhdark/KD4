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
use crate::tools::command_output_artifact::ReadToolOutputResult;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::command_output_artifact::select_file_snapshot_for_script;
use crate::tools::command_output_artifact::select_file_snapshot_with_ceiling;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS;
use crate::tools::handlers::read_tool_output_spec::file_selector_schema;
use crate::tools::handlers::read_tool_output_spec::read_tool_output_output_schema;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const MAX_FILE_MIB: usize = 8;
const MAX_FILE_BYTES: usize = MAX_FILE_MIB * 1024 * 1024;

pub(crate) struct ReadFileHandler;

struct PendingSourceInspection {
    store: std::sync::Arc<codex_agent_task_store::LocalAgentTaskStore>,
    start: codex_agent_task_store::SourceInspectionStart,
}

#[derive(Deserialize)]
#[serde(try_from = "RawReadFileArgs")]
struct ReadFileArgs {
    path: String,
    environment_id: Option<String>,
    selectors: Option<Vec<ToolOutputSelector>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReadFileArgs {
    #[serde(alias = "file_path")]
    path: String,
    environment_id: Option<String>,
    selectors: Option<Vec<ToolOutputSelector>>,
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
            Some(vec![ToolOutputSelector::Lines { start, end }])
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
    mut output: serde_json::Value,
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
    let selected = if previous["selectors"].is_null()
        && output["file_complete"] == true
        && selectors.iter().any(|selector| matches!(selector["kind"].as_str(), Some("lines" | "search")))
    {
        // Default reads use a byte selector. Reuse that whole authenticated
        // source through the owning selector engine instead of requiring another
        // filesystem read for lines/search. Search hydration stays self-contained.
        let source = retained.first()?;
        let text = source["text"].as_str()?;
        if retained.len() != 1 || source["status"] != "ok" || source["complete"] != true
            || source["canonical_range"]["start"] != 0
            || source["canonical_range"]["end"] != output["canonical_bytes"]
            || output["canonical_bytes"].as_u64()? != text.len() as u64
        { return None; }
        let canonical = CanonicalToolResult::text(text.to_owned());
        if output["canonical_sha256"].as_str()? != canonical.sha256 { return None; }
        let selectors: Vec<ToolOutputSelector> = serde_json::from_value(json!(selectors)).ok()?;
        if selectors.iter().any(|selector| !matches!(selector,
            ToolOutputSelector::Bytes { .. } | ToolOutputSelector::Lines { .. } | ToolOutputSelector::Search { .. }
        )) { return None; }
        let (selection, continuation) = select_file_snapshot_for_script(&canonical, Some(selectors.clone())).ok()?;
        if !selection.complete || continuation.is_some() { return None; }
        let selected = serde_json::to_value(selection.results).ok()?;
        // The replay ledger is shared by direct and script consumers. Direct
        // selection can reorder/merge mixed ranges and has a smaller budget.
        // Until the ledger carries that consumer policy, reuse only projections
        // with identical complete results in both modes, never change the API
        // shape merely to save an I/O operation.
        let (direct, _) = select_file_snapshot_with_ceiling(
            &canonical, Some(selectors), RECOVERY_AGGREGATE_TOKEN_CEILING.saturating_sub(1_000),
        ).ok()?;
        if !direct.complete || serde_json::to_value(direct.results).ok()? != selected { return None; }
        selected
    } else {
        // Partial search results can reference other hydration owners. Do not
        // reuse them after removing those owners or infer undelivered source.
        if selectors.iter().any(|selector| !matches!(selector["kind"].as_str(), Some("bytes" | "lines"))) {
            return None;
        }
        let mut selected = Vec::new();
        for selector in selectors {
            selected.push(retained.iter().find_map(|result| retained_read_selection(result, selector))?);
        }
        json!(selected)
    };
    output["results"] = json!(selected);
    output["complete"] = json!(true);
    output["delivered_selection_complete"] = json!(true);
    // Complete inline reads use null instead of a retained artifact identity.
    let mut typed = output.clone();
    if typed["artifact_id"].is_null() { typed["artifact_id"] = json!(""); }
    let typed = serde_json::from_value::<ReadToolOutputResult>(typed).ok()?;
    output["file_complete"] = json!(file_selection_complete(&typed));
    let fields = output.as_object_mut()?;
    for key in ["continuation", "page_selectors", "criterion_evidence", "inspection_proof_error"] {
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
        let mut selectors = JsonSchema::array(file_selector_schema(), Some("Omit to read the first page immediately. Batch known ranges of this file here rather than calling once per range. Search results include matching text in results[].value.hydrated_ranges: consume that text before requesting another inspection. Explicit selectors are exact; oversized selections return child_selectors for the retained snapshot.".to_string()));
        selectors.min_items = Some(1);
        selectors.max_items = Some(READ_TOOL_OUTPUT_MAX_SELECTORS as u64);
        let mut output = read_tool_output_output_schema(file_selector_schema());
        output["properties"]["path"] = json!({"type": "string"});
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
        output["properties"]["continuation"] =
            serde_json::to_value(file_selector_schema()).unwrap_or_default();
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
                ("path".to_string(), JsonSchema::string(Some("File path, relative to the environment cwd or absolute, a `skill:` locator, `skill:catalog` for complete enabled skill metadata, or `context:desktop` for full configured Desktop guidance. Omit environment_id for host-owned locators. Use search selectors to discover newly relevant guidance.".to_string()))),
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
                    ToolOutputSelector::Bytes { .. }
                        | ToolOutputSelector::Lines { .. }
                        | ToolOutputSelector::Search { .. }
                )
            }) {
                return Err(FunctionCallError::RespondToModel(
                    "read_file supports bytes, lines, and search selectors".to_string(),
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
            let (contents, resolved_path, inspection) = if args.path
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
                (contents.to_string(), args.path.clone(), None)
            } else if args
                .path
                .starts_with(codex_core_skills::SKILL_CATALOG_LOCATOR_PREFIX)
            {
                let (contents, path) = read_skill_locator(&invocation, &args).await?;
                (contents, path, None)
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
            let (canonical, mut result, mut continuation, page_selectors, total_lines) =
                tokio::task::spawn_blocking(move || {
                    let total_lines = contents.lines().count();
                    let canonical = CanonicalToolResult::text(contents);
                    let selection = if script_consumer {
                        select_file_snapshot_for_script(&canonical, args.selectors)
                    } else {
                        select_file_snapshot_with_ceiling(&canonical, args.selectors, token_ceiling)
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
                            (canonical, result, continuation, pages, total_lines)
                        })
                })
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "file selection worker failed: {err}"
                    ))
                })?
                .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
            let file_complete = file_selection_complete(&result);
            // Explicit selections retain the existing immutable-snapshot contract.
            // Complete default reads stay inline; omitted bytes need one durable
            // snapshot, but selecting them never rereads the file just written.
            let artifact = if explicit_selection || continuation.is_some() {
                let reused = if let Some(id) = invocation.session
                    .reusable_tool_artifact(canonical.exact_bytes, &canonical.sha256).await
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
                crate::tools::context::declared_file_lineage_evidence(&canonical.bytes)
                    .unwrap_or_else(|| json!({
                "source": "read_file",
                "scope": {
                    "path": resolved_path,
                    "environment": if args.path.starts_with(codex_core_skills::SKILL_CATALOG_LOCATOR_PREFIX) {
                        "host-skills"
                    } else if args.path == crate::context::desktop_instructions::LOCATOR {
                        "host-context"
                    } else {
                        args.environment_id.as_deref()
                            .or_else(|| invocation.step_context.environments.primary()
                                .map(|environment| environment.environment_id.as_str()))
                            .unwrap_or("primary")
                    },
                },
                "identity": identity,
                    }))
            });
            let mut output = serde_json::to_value(result)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            output["artifact_id"] = json!(artifact_id);
            output["retained_artifact_complete"] = json!(artifact_id.is_some());
            output["path"] = json!(resolved_path);
            output["total_lines"] = json!(total_lines);
            output["source_sha256"] = json!(canonical.sha256);
            output["file_complete"] = json!(file_complete);
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
            let projected = codex_code_mode::model_visible_tool_result(
                &ToolName::plain("read_file"), &output,
            );
            let mut output = JsonToolOutput::new(output);
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
async fn read_environment_file(
    invocation: &ToolInvocation,
    args: &ReadFileArgs,
) -> Result<(String, String, Option<PendingSourceInspection>), FunctionCallError> {
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
            let metadata = fs
                .get_metadata(&path, Some(&sandbox))
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "unable to locate {}: {err}",
                        path.inferred_native_path_string()
                    ))
                })?;
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
    Ok((contents, path.inferred_native_path_string(), inspection))
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

    #[test]
    fn legacy_read_file_arguments_preserve_line_semantics() {
        let args: ReadFileArgs = parse_arguments(r#"{"file_path":"a.rs","offset":3,"limit":2}"#).unwrap();
        assert_eq!(args.path, "a.rs");
        assert_eq!(args.selectors, Some(vec![ToolOutputSelector::Lines { start: 3, end: 4 }]));
        for invalid in [
            r#"{"file_path":"a.rs","offset":0}"#,
            r#"{"file_path":"a.rs","limit":0}"#,
            r#"{"file_path":"a.rs","offset":1,"selectors":[]}"#,
        ] {
            assert!(parse_arguments::<ReadFileArgs>(invalid).is_err());
        }
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
        assert_eq!(result["results"].as_array().unwrap().len(), 3);
        for selection in result["results"].as_array().unwrap() {
            assert_eq!(selection["text"], line);
        }
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
    async fn report_reads_reuse_lineage_but_changed_or_unsigned_files_do_not() {
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
            assert_eq!(first.sampling_request_signal().unwrap()["semantic_evidence"], expected);
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
                assert_eq!(result["results"][0]["continuation"], selector);
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
                arguments: json!({"artifact_id": artifact_id, "selectors": [selector]}).to_string(),
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
                json!([{"kind": "json_pointer", "pointer": ""}]),
                false,
                false,
                "supports bytes, lines, and search",
            ),
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
        let spec = serde_json::to_value(ReadFileHandler.spec()).unwrap();
        let validator = jsonschema::validator_for(&spec["parameters"]).unwrap();
        assert!(!validator.is_valid(
            &json!({"path": "input.txt", "selectors": [{"kind": "section", "id": "x"}]})
        ));
        assert!(validator.is_valid(
            &json!({"path": "input.txt", "selectors": [{"kind": "lines", "start": 1, "end": 2}]})
        ));
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
}
