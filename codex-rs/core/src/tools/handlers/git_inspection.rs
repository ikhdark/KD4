use std::collections::BTreeMap;

use codex_git_utils::DiffComparison;
use codex_git_utils::DiffInspection;
use codex_git_utils::DiffRequest;
use codex_git_utils::DiffSourceRequest;
use codex_git_utils::RangeTraversal;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct GitInspectionHandler {
    pub(crate) range: bool,
}

/// The common authorization/startup boundary for native read-only Git tools.
/// Keep operation schemas and cancellation owners separate: status may wait for
/// changes, while the other operations are bounded immediate observations.
pub(super) async fn prepare_git_read(
    invocation: &ToolInvocation,
    environment_id: Option<&str>,
    label: &str,
    restriction: &str,
) -> Result<TurnEnvironment, FunctionCallError> {
    let environment = wait_for_tool_environment(
        &invocation.step_context.environments,
        environment_id,
        &invocation.cancellation_token,
    )
    .await?
    .ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "{label} requires a selected local environment"
        ))
    })?;
    if environment.environment.is_remote()
        || !invocation
            .step_context
            .turn
            .file_system_sandbox_policy()
            .has_full_disk_read_access()
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "{label} requires a local environment with full disk read access; {restriction}"
        )));
    }
    crate::tools::parallel::wait_for_workspace_baseline().await;
    Ok(environment)
}

/// Preserve the same exact-result contract for direct and Code Mode calls.
pub(super) fn git_read_output(
    mut value: serde_json::Value,
    environment_id: &str,
    invocation: &ToolInvocation,
    delivery_error: impl FnOnce() -> String,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    value["environment_id"] = json!(environment_id);
    let output = JsonToolOutput::new(value);
    crate::tools::context::ensure_exact_model_delivery(&output, invocation)
        .map_err(|_| FunctionCallError::RespondToModel(delivery_error()))?;
    Ok(boxed_tool_output(output))
}

/// Omitted (max_patch_bytes, max_files) keep a complete direct response within
/// the default ~10k-token (~40 KB JSON) delivery budget; Code Mode may ask more.
fn default_limits(range: bool) -> (usize, usize) {
    if range {
        (8 * 1024, 30)
    } else {
        (16 * 1024, 50)
    }
}
const DEFAULT_MAX_COMMITS: usize = 50;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    comparison: Option<DiffComparison>,
    #[serde(default)]
    layer_view: bool,
    if_layer_sha256: Option<String>,
    #[serde(default)]
    include_history: bool,
    #[serde(default)]
    use_merge_base: bool,
    #[serde(default)]
    select_after_renames: bool,
    base: Option<String>,
    head: Option<String>,
    literal_history: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    max_patch_bytes: Option<usize>,
    max_files: Option<usize>,
    source_ranges: Option<DiffSourceRequest>,
    traversal: Option<RangeTraversal>,
    max_commits: Option<usize>,
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for GitInspectionHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(if self.range {
            "git_commit_range"
        } else {
            "git_diff"
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let (default_patch, default_files) = default_limits(self.range);
        let bounded = |minimum: i32, maximum: i32, description: String| JsonSchema {
            minimum: Some(minimum.into()),
            maximum: Some(maximum.into()),
            ..JsonSchema::integer(Some(description))
        };
        let mut properties = BTreeMap::from([
            ("base".to_string(), JsonSchema::string(Some("Base commit revision, resolved once to an immutable commit/tree; required for revision comparisons.".into()))),
            ("head".to_string(), JsonSchema::string(Some("Head commit revision, resolved once; by default the diff is base tree to head tree.".into()))),
            ("paths".to_string(), JsonSchema {
                max_items: Some(64),
                ..JsonSchema::array(JsonSchema::string(None), Some("Literal, case-sensitive repository-root-relative file/directory paths; omitted means all tracked changes. Paths matching nothing at either endpoint are listed in unmatched_paths. For ranges, limits endpoint diff only, never commit selection.".into()))
            }),
            ("max_patch_bytes".to_string(), bounded(0, 131_072, format!("Maximum returned patch text bytes; default {default_patch}. Hex patches carry half as many raw bytes. 0 returns metadata only. A truncated patch is an exact prefix and may end mid-hunk."))),
            ("max_files".to_string(), bounded(1, 1000, format!("Maximum file records returned; default {default_files}. Totals cover the full selected endpoint diff, even if records/patch are truncated."))),
            ("environment_id".to_string(), JsonSchema::string(Some("Selected local execution environment; full disk read access required.".into()))),
        ]);
        properties.insert("traversal".to_string(), JsonSchema::string_enum(vec![json!("all_parents"), json!("first_parent")], Some("Default all_parents. History only: selects base..head reachability traversal; merges included once, no per-commit diffs/statistics are summed.".into())));
        properties.insert("max_commits".to_string(), bounded(1, 1000, format!("History only: maximum topologically ordered commit records returned; default {DEFAULT_MAX_COMMITS}. total_commits is separate.")));
        let required = if self.range {
            vec!["base".to_string(), "head".to_string()]
        } else {
            properties.insert("comparison".to_string(), JsonSchema::string_enum(vec![json!("staged"), json!("unstaged"), json!("revisions")], Some("Required. staged: captured HEAD (or unborn empty tree) to live index; unstaged: live index to working tree; revisions: explicit base/head commit trees. Never combines staged and unstaged.".into())));
            properties.insert("literal_history".to_string(), JsonSchema::string(Some("Opt-in Git text patch history for one case-sensitive literal of 1–256 UTF-8 bytes, no CR/LF/NUL. Requires comparison:revisions, head and 1–64 literal paths, without base or diff/history options other than max_commits (1–100, default 50). Counts added and deleted matching lines, not occurrences or context. Uses Git path-limited history, no merge diffs or rename following; refuses ambiguous or shallow history.".into())));
            properties.insert("include_history".to_string(), JsonSchema::boolean(Some("Default false. With comparison:revisions and explicit base/head, also summarize base..head commits, ancestry/merge bases and shallow-history uncertainty. Returns the commit-range result with endpoint_diff separate from reachability history; paths limit only that diff. Enables traversal/max_commits, excludes source_ranges, and defaults max_patch_bytes/max_files to 8192/30 instead of 16384/50. Reversions can have commits but no endpoint changes; statistics are never summed across commits.".into())));
            properties.insert("use_merge_base".to_string(), JsonSchema::boolean(Some("Default false. For revisions without include_history, compare the sole common ancestor tree to the captured head tree. Returns the original requested base as merge_base_selection.requested_base; from identifies the ancestor. Shallow, unrelated, ambiguous or unavailable ancestry is refused; no fetches.".into())));
            properties.insert("select_after_renames".to_string(), JsonSchema::boolean(Some("Default false. For revisions without include_history and with paths, detect renames across the full captured diff before selecting literal old or new endpoints. Full capture limits still apply; displayed totals and source ranges describe selected files.".into())));
            properties.insert("source_ranges".to_string(), JsonSchema::object(BTreeMap::from([
                ("context_lines".into(), bounded(0, 100, "Surrounding lines per hunk side; default 3. Both source and patch use this context.".into())),
                ("max_bytes".into(), bounded(0, 131_072, "Maximum combined old/new source text bytes; default 16384. Whole hunks only, at most 1000 returned; omitted ranges remain explicitly incomplete. Independent of max_patch_bytes.".into())),
                ("if_diff_sha256".into(), JsonSchema::string(Some("Optional diff_sha256 from a previous git_diff with identical paths/context. Refuse stale coordinates if any captured content, endpoint commit or metadata differs. Omit only when requesting newly resolved hunks rather than reusing prior coordinates.".into()))),
            ]), None, Some(false.into())));
            properties.insert("layer_view".into(), JsonSchema::boolean(Some("Default false. Opt in to the exact file layer observation using comparison:staged and one paths entry instead of diff options. Returns raw hex bytes and SHA-256 as read (including CRLF); committed and index layers include Git OID/mode. Missing layers are null. Regular files only, up to 4096 bytes per layer. Non-atomic; observation checks may refuse detected changes. Git administrative/nested paths and unmerged entries are refused.".into())));
            properties.insert("if_layer_sha256".into(), JsonSchema::string(Some("Optional observation_sha256 from a prior layer_view of the same exact path. Refuse a changed observation; this is not an atomic snapshot token.".into())));
            vec!["comparison".to_string()]
        };
        let purpose = if self.range {
            "Summarize base..head commits separately from the base-tree to head-tree endpoint diff. Reversions can produce commits with zero endpoint changes. Ancestry booleans, all merge bases, shallow-history uncertainty and all-parent/first-parent traversal are explicit. Non-ancestor ranges are allowed; paths never filter commits. No per-commit statistics are summed."
        } else {
            "Inspect one explicit Git comparison: staged, unstaged, or two immutable revisions. For staged with exactly one literal path, layer_view:true is a separate non-atomic observation of committed, index and raw worktree contents, including untracked or missing layers. For revisions, use_merge_base:true selects the sole common ancestor instead of the supplied base as the left diff endpoint; it cannot be combined with include_history. For revisions, include_history:true also returns the base..head commit-range summary with endpoint_diff separate from commit history; traversal/max_commits apply only to history. Otherwise optional source_ranges resolves hunks into exact old/new source text and line ranges with configurable context from the SAME Git capture, binding each side to its path/mode/blob OID, including deletions and renames. Zero line_count denotes an anchor, not a source line. Working-tree blob IDs identify captured normalized content, not stored/recoverable objects or current raw bytes. diff_sha256 binds the full capture even when patch/files are truncated; use source_ranges.if_diff_sha256 with matching paths/context to reject stale coordinates. Binary/non-UTF-8/non-regular source ranges remain explicitly unavailable; pure renames/mode changes have no source hunks. Coverage is only changed lines with context, never complete behavioral review or read_file history/retention credit. Returns comparison endpoints, status/modes/raw OIDs, rename similarity, binary flags and exact Git patch hunks; never reconstructs diffs from turn patch tracking."
        };
        ToolSpec::Function(ResponsesApiTool {
            name: if self.range {
                "git_commit_range"
            } else {
                "git_diff"
            }
            .into(),
            description: format!(
                "{purpose} Ordinary diffs exclude untracked files. Mutable index/working-tree endpoints are observations, not coherent snapshots or content identities; raw all-zero OIDs mean unavailable. Unstaged comparisons cover checked-out submodule commits, not dirty submodule work trees. Patch encoding is utf8, or hex for non-UTF-8 bytes. Full selected totals remain separate from explicit file/patch/commit truncation; bounded stderr warnings keep their start and end, including Git's rename-limit diagnostics, and mark omitted bytes. Rename detection uses 50% similarity and a 1000-candidate limit. Git's built-in EOL/encoding normalization is preserved, but configured clean/process filters, external diff and textconv are disabled. Local full-disk-read environments only. No fetches or repository writes. Unsupported/missing history, conflicts, non-UTF-8 paths, timeout or capture above 8 MiB fail explicitly, never as no changes. Default limits fit the default direct-response budget; larger limits suit Code Mode, and a direct response that cannot fit fails undelivered. Use narrower paths for oversized diffs; there is no retained patch recovery handle."
            ),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(properties, Some(required), Some(false.into())),
            output_schema: Some(if self.range {
                output_schema(true)
            } else {
                json!({"oneOf": [output_schema(false), output_schema(true), layer_output_schema(), literal_history_schema()]})
            }.into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        let inspected = async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "Git inspection requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let range = self.range || args.include_history;
            let literal_history = args.literal_history.is_some();
            if self.range && (args.comparison.is_some() || args.include_history)
                || range && args.source_ranges.is_some()
                || !range && (args.traversal.is_some() || !literal_history && args.max_commits.is_some())
            {
                return Err(FunctionCallError::RespondToModel(
                    "arguments do not match the selected Git inspection tool".into(),
                ));
            }
            let comparison = if self.range {
                DiffComparison::Revisions
            } else {
                args.comparison.ok_or_else(|| {
                    FunctionCallError::RespondToModel("comparison is required".into())
                })?
            };
            if literal_history
                && (range
                    || comparison != DiffComparison::Revisions
                    || args.base.is_some()
                    || args.use_merge_base
                    || args.select_after_renames
                    || args.max_patch_bytes.is_some()
                    || args.max_files.is_some()
                    || args.source_ranges.is_some()
                    || args.traversal.is_some())
            {
                return Err(FunctionCallError::RespondToModel(
                    "literal_history requires comparison:revisions with head and paths; only max_commits may be combined".into(),
                ));
            }
            if args.include_history && !matches!(comparison, DiffComparison::Revisions) {
                return Err(FunctionCallError::RespondToModel(
                    "include_history requires comparison:revisions with base and head".into(),
                ));
            }
            if args.use_merge_base && (range || comparison != DiffComparison::Revisions) {
                return Err(FunctionCallError::RespondToModel(
                    "use_merge_base requires comparison:revisions without include_history".into(),
                ));
            }
            if args.if_layer_sha256.is_some() && !args.layer_view
                || args.layer_view
                    && (range || literal_history || comparison != DiffComparison::Staged || args.use_merge_base
                        || args.select_after_renames || args.paths.len() != 1 || args.base.is_some() || args.head.is_some()
                        || args.source_ranges.is_some() || args.max_files.is_some()
                        || args.max_patch_bytes.is_some())
            {
                return Err(FunctionCallError::RespondToModel(
                    "layer_view requires comparison:staged, one exact paths entry, and no diff options".into(),
                ));
            }
            let environment = prepare_git_read(
                &invocation,
                args.environment_id.as_deref(),
                "Git inspection",
                "remote/restricted reads are unsupported",
            )
            .await?;
            let cwd = environment
                .cwd()
                .to_abs_path()
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                .to_path_buf();
            if args.layer_view {
                let operation = DiffInspection::inspect_path_layers(
                    cwd,
                    args.paths.into_iter().next().expect("one validated path"),
                    args.if_layer_sha256,
                );
                let layers = tokio::select! {
                    _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("Git layer inspection cancelled; no repository writes were requested".into())),
                    result = operation => result.map_err(FunctionCallError::RespondToModel)?,
                };
                let value = serde_json::to_value(layers)
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                return git_read_output(value, &environment.environment_id, &invocation, || {
                    "Git layer observation exceeds the delivery budget; nothing was delivered".into()
                });
            }
            let (default_patch, default_files) = default_limits(range);
            if args.select_after_renames
                && (range || comparison != DiffComparison::Revisions || args.paths.is_empty())
            {
                return Err(FunctionCallError::RespondToModel(
                    "select_after_renames requires comparison:revisions without include_history and with paths".into(),
                ));
            }
            let request = DiffRequest {
                comparison,
                use_merge_base: args.use_merge_base,
                select_after_renames: args.select_after_renames,
                base: args.base,
                head: args.head,
                paths: args.paths,
                max_patch_bytes: args.max_patch_bytes.unwrap_or(default_patch),
                max_files: args.max_files.unwrap_or(default_files),
                source_ranges: args.source_ranges,
            };
            let operation = async {
                if let Some(literal) = args.literal_history {
                    serde_json::to_value(
                        request
                            .inspect_literal_history(
                                cwd,
                                literal,
                                args.max_commits.unwrap_or(DEFAULT_MAX_COMMITS),
                            )
                            .await?,
                    )
                } else if range {
                    serde_json::to_value(
                        codex_git_utils::summarize_commit_range(
                            cwd,
                            request,
                            args.traversal.unwrap_or(RangeTraversal::AllParents),
                            args.max_commits.unwrap_or(DEFAULT_MAX_COMMITS),
                        )
                        .await?,
                    )
                } else {
                    serde_json::to_value(codex_git_utils::inspect_diff(cwd, request).await?)
                }
                .map_err(|error| error.to_string())
            };
            let output = tokio::select! {
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("Git inspection cancelled; no repository writes were requested".into())),
                result = operation => result.map_err(FunctionCallError::RespondToModel)?,
            };
            git_read_output(
                output,
                &environment.environment_id,
                &invocation,
                || format!(
                    "Git inspection result exceeds the current model delivery budget; nothing was delivered. Retry with smaller {} or narrower paths.",
                    if literal_history {
                        "max_commits"
                    } else if range {
                        "max_patch_bytes, max_files or max_commits"
                    } else {
                        "max_patch_bytes or max_files"
                    }
                ),
            )
        };
        // Native Git reads request no fetches or repository writes; a legacy
        // failure leaves no effects to recover.
        Box::pin(async move {
            inspected
                .await
                .map_err(super::read_tool_output::settled_recovery_failure)
        })
    }
}

impl CoreToolRuntime for GitInspectionHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}

fn layer_output_schema() -> serde_json::Value {
    let string = json!({"type":"string"});
    let integer = json!({"type":"integer","minimum":0,"maximum":4096});
    let blob = json!({"type":["object","null"],"additionalProperties":false,
        "properties":{"mode":string,"oid":string,"bytes":integer,"data_hex":string,"sha256":string},
        "required":["mode","oid","bytes","data_hex","sha256"]});
    json!({"type":"object","additionalProperties":false,"properties":{
        "repository_root":string,"path":string,"commit_oid":string,"observation_sha256":string,
        "atomic_snapshot":{"const":false},"committed":blob,"index":blob,"environment_id":string,
        "worktree":{"type":["object","null"],"additionalProperties":false,
            "properties":{"bytes":integer,"data_hex":string,"sha256":string},
            "required":["bytes","data_hex","sha256"]}},
        "required":["repository_root","path","commit_oid","observation_sha256","atomic_snapshot",
            "committed","index","environment_id","worktree"]})
}

fn literal_history_schema() -> serde_json::Value {
    let string = json!({"type":"string"});
    let count = json!({"type":"integer","minimum":0});
    json!({"type":"object","additionalProperties":false,"properties":{
        "environment_id":string,"head_commit_oid":string,"literal":string,
        "paths":{"type":"array","items":string},
        "commits_truncated":{"type":"boolean"},
        "commits":{"type":"array","items":{"type":"object","additionalProperties":false,
            "properties":{"commit_oid":string,"added_matching_lines":count,"removed_matching_lines":count,
                "direction":{"type":"string","enum":["introduced","removed","mixed"]}},
            "required":["commit_oid","added_matching_lines","removed_matching_lines","direction"]}}},
        "required":["environment_id","head_commit_oid","literal","paths","commits","commits_truncated"]})
}

fn output_schema(range: bool) -> serde_json::Value {
    let string = json!({"type":"string"});
    let boolean = json!({"type":"boolean"});
    let integer = json!({"type":"integer","minimum":0});
    let strings = json!({"type":"array","items":string});
    let nullable_string = json!({"type":["string","null"]});
    let endpoint = json!({"type":"object","additionalProperties":false,
        "properties":{"kind":string,"revision":nullable_string,"commit_oid":nullable_string,"tree_oid":nullable_string},
        "required":["kind","revision","commit_oid","tree_oid"]});
    let file = json!({"type":"object","additionalProperties":false,"properties":{
        "status":string,"old_path":string,"new_path":string,"old_mode":string,"new_mode":string,
        "old_oid":string,"new_oid":string,"similarity_percent":{"type":["integer","null"]},
        "binary":boolean,"additions":{"type":["integer","null"]},"deletions":{"type":["integer","null"]}},
        "required":["status","old_path","new_path","old_mode","new_mode","old_oid","new_oid","similarity_percent","binary","additions","deletions"]});
    let mut diff = json!({"type":"object","additionalProperties":false,"properties":{
        "repository_root":string,"comparison":{"type":"string","enum":["staged","unstaged","revisions"]},
        "from":endpoint,"to":endpoint,"paths":strings,"unmatched_paths":strings,"git_args":strings,"filter_policy":string,"path_selection":{"type":"string","enum":["post_rename"]},
        "untracked_included":boolean,"coherent_snapshot_verified":boolean,
        "total_files":integer,"files":{"type":"array","items":file},"files_truncated":boolean,
        "additions":integer,"deletions":integer,"binary_files":integer,"patch":string,
        "patch_encoding":{"type":"string","enum":["utf8","hex"]},"total_patch_bytes":integer,
        "returned_patch_bytes":integer,"patch_truncated":boolean,"warnings":string,
        "diff_sha256":string},
        "required":["repository_root","comparison","from","to","paths","unmatched_paths","git_args","filter_policy","untracked_included","coherent_snapshot_verified","total_files","files","files_truncated","additions","deletions","binary_files","patch","patch_encoding","total_patch_bytes","returned_patch_bytes","patch_truncated","warnings","diff_sha256"]});
    let side = json!({"type":"object","additionalProperties":false,"properties":{
        "path":string,"mode":string,"blob_oid":nullable_string,"present":boolean},
        "required":["path","mode","blob_oid","present"]});
    let source_range = json!({"type":"object","additionalProperties":false,"properties":{
        "start_line":integer,"line_count":integer,"text":string},
        "required":["start_line","line_count","text"]});
    diff["properties"]["source_ranges"] = json!({"type":"object","additionalProperties":false,"properties":{
        "coverage":{"const":"changed_lines_with_context"},"behavioral_review_complete":{"const":false},
        "coordinate_space":{"const":"git_blob_lines_after_builtin_normalization"},
        "context_lines":integer,"files_truncated":boolean,"ranges_complete":boolean,"returned_source_bytes":integer,
        "files":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":{
            "old":side,"new":side,"total_hunks":integer,"ranges_complete":boolean,
            "unavailable_reason":nullable_string,
            "hunks":{"type":"array","items":{"type":"object","additionalProperties":false,
                "properties":{"old":source_range,"new":source_range},"required":["old","new"]}}},
            "required":["old","new","hunks","total_hunks","ranges_complete","unavailable_reason"]}}},
        "required":["coverage","behavioral_review_complete","coordinate_space","context_lines","files","files_truncated","ranges_complete","returned_source_bytes"]});
    if !range {
        diff["properties"]["merge_base_selection"] = json!({
            "type":"object","additionalProperties":false,"properties":{"requested_base":endpoint},
            "required":["requested_base"]
        });
        diff["properties"]["environment_id"] = string;
        if let Some(required) = diff["required"].as_array_mut() {
            required.push(json!("environment_id"));
        }
        return diff;
    }
    json!({"type":"object","additionalProperties":false,"properties":{
        "environment_id":string,"endpoint_diff":diff,
        "traversal":{"type":"string","enum":["all_parents","first_parent"]},
        "selection":string,"merge_handling":string,"merge_bases":strings,
        "base_is_ancestor":{"type":["boolean","null"]},"head_is_ancestor":{"type":["boolean","null"]},
        "history_complete":boolean,"total_commits":integer,"commits_truncated":boolean,"statistics_scope":string,
        "commits":{"type":"array","items":{"type":"object","additionalProperties":false,
            "properties":{"commit_oid":string,"parent_oids":strings,"is_merge":boolean,"subject":string},
            "required":["commit_oid","parent_oids","is_merge","subject"]}}},
        "required":["environment_id","endpoint_diff","traversal","selection","merge_handling","merge_bases","base_is_ancestor","head_is_ancestor","history_complete","total_commits","commits","commits_truncated","statistics_scope"]})
}
