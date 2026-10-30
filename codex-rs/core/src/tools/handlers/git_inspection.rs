use std::collections::BTreeMap;

use codex_git_utils::DiffComparison;
use codex_git_utils::DiffRequest;
use codex_git_utils::RangeTraversal;
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

pub(crate) struct GitInspectionHandler {
    pub(crate) range: bool,
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
    base: Option<String>,
    head: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    max_patch_bytes: Option<usize>,
    max_files: Option<usize>,
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
            ("head".to_string(), JsonSchema::string(Some("Head commit revision, resolved once; endpoint diff is base tree to head tree, not merge-base to head.".into()))),
            ("paths".to_string(), JsonSchema {
                max_items: Some(64),
                ..JsonSchema::array(JsonSchema::string(None), Some("Literal, case-sensitive repository-root-relative file/directory paths; omitted means all tracked changes. Paths matching nothing at either endpoint are listed in unmatched_paths. For ranges, limits endpoint diff only, never commit selection.".into()))
            }),
            ("max_patch_bytes".to_string(), bounded(0, 131_072, format!("Maximum returned patch text bytes; default {default_patch}. Hex patches carry half as many raw bytes. 0 returns metadata only. A truncated patch is an exact prefix and may end mid-hunk."))),
            ("max_files".to_string(), bounded(1, 1000, format!("Maximum file records returned; default {default_files}. Totals cover the full selected endpoint diff, even if records/patch are truncated."))),
            ("environment_id".to_string(), JsonSchema::string(Some("Selected local execution environment; full disk read access required.".into()))),
        ]);
        let required = if self.range {
            properties.insert("traversal".to_string(), JsonSchema::string_enum(vec![json!("all_parents"), json!("first_parent")], Some("Default all_parents. Selects base..head reachability traversal; merges included once, no per-commit diffs/statistics are summed.".into())));
            properties.insert("max_commits".to_string(), bounded(1, 1000, format!("Maximum topologically ordered commit records returned; default {DEFAULT_MAX_COMMITS}. total_commits is separate.")));
            vec!["base".to_string(), "head".to_string()]
        } else {
            properties.insert("comparison".to_string(), JsonSchema::string_enum(vec![json!("staged"), json!("unstaged"), json!("revisions")], Some("Required. staged: captured HEAD (or unborn empty tree) to live index; unstaged: live index to working tree; revisions: explicit base/head commit trees. Never combines staged and unstaged.".into())));
            vec!["comparison".to_string()]
        };
        let purpose = if self.range {
            "Summarize base..head commits separately from the base-tree to head-tree endpoint diff. Reversions can produce commits with zero endpoint changes. Ancestry booleans, all merge bases, shallow-history uncertainty and all-parent/first-parent traversal are explicit. Non-ancestor ranges are allowed; paths never filter commits. No per-commit statistics are summed."
        } else {
            "Inspect one explicit Git comparison: staged, unstaged, or two immutable revisions. Returns comparison endpoint identities, file status/modes/raw OIDs, rename similarity, binary flags and Git-generated exact unified patch hunks. Does not reconstruct diffs from turn patch tracking."
        };
        ToolSpec::Function(ResponsesApiTool {
            name: if self.range { "git_commit_range" } else { "git_diff" }.into(),
            description: format!("{purpose} Untracked files are excluded. Mutable index/working-tree endpoints are observations, not coherent snapshots or content identities; raw all-zero OIDs mean unavailable. Unstaged comparisons cover checked-out submodule commits, not dirty submodule work trees. Patch encoding is utf8, or hex for non-UTF-8 bytes. Full selected totals remain separate from explicit file/patch/commit truncation; bounded stderr warnings keep their start and end, including Git's rename-limit diagnostics, and mark omitted bytes. Rename detection uses 50% similarity and a 1000-candidate limit. Git's built-in EOL/encoding normalization is preserved, but configured clean/process filters, external diff and textconv are disabled. Local full-disk-read environments only. No fetches or repository writes. Unsupported/missing history, conflicts, non-UTF-8 paths, timeout or capture above 8 MiB fail explicitly, never as no changes. Default limits fit the default direct-response budget; larger limits suit Code Mode, and a direct response that cannot fit fails undelivered. Use narrower paths for oversized diffs; there is no retained patch recovery handle."),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(properties, Some(required), Some(false.into())),
            output_schema: Some(output_schema(self.range).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "Git inspection requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            if self.range && args.comparison.is_some()
                || !self.range && (args.traversal.is_some() || args.max_commits.is_some())
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
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "Git inspection requires a selected local environment".into(),
                )
            })?;
            if environment.environment.is_remote()
                || !invocation
                    .step_context
                    .turn
                    .file_system_sandbox_policy()
                    .has_full_disk_read_access()
            {
                return Err(FunctionCallError::RespondToModel("Git inspection requires a local environment with full disk read access; remote/restricted reads are unsupported".into()));
            }
            crate::tools::parallel::wait_for_workspace_baseline().await;
            let cwd = environment
                .cwd()
                .to_abs_path()
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                .to_path_buf();
            let (default_patch, default_files) = default_limits(self.range);
            let request = DiffRequest {
                comparison,
                base: args.base,
                head: args.head,
                paths: args.paths,
                max_patch_bytes: args.max_patch_bytes.unwrap_or(default_patch),
                max_files: args.max_files.unwrap_or(default_files),
            };
            let operation = async {
                if self.range {
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
            let mut output = tokio::select! {
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("Git inspection cancelled; no repository writes were requested".into())),
                result = operation => result.map_err(FunctionCallError::RespondToModel)?,
            };
            output["environment_id"] = json!(environment.environment_id);
            let output = JsonToolOutput::new(output);
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation).map_err(
                |_| {
                    FunctionCallError::RespondToModel(format!(
                        "Git inspection result exceeds the current model delivery budget; nothing was delivered. Retry with smaller {} or narrower paths.",
                        if self.range {
                            "max_patch_bytes, max_files or max_commits"
                        } else {
                            "max_patch_bytes or max_files"
                        }
                    ))
                },
            )?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for GitInspectionHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
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
        "from":endpoint,"to":endpoint,"paths":strings,"unmatched_paths":strings,"git_args":strings,"filter_policy":string,
        "untracked_included":boolean,"coherent_snapshot_verified":boolean,
        "total_files":integer,"files":{"type":"array","items":file},"files_truncated":boolean,
        "additions":integer,"deletions":integer,"binary_files":integer,"patch":string,
        "patch_encoding":{"type":"string","enum":["utf8","hex"]},"total_patch_bytes":integer,
        "returned_patch_bytes":integer,"patch_truncated":boolean,"warnings":string},
        "required":["repository_root","comparison","from","to","paths","unmatched_paths","git_args","filter_policy","untracked_included","coherent_snapshot_verified","total_files","files","files_truncated","additions","deletions","binary_files","patch","patch_encoding","total_patch_bytes","returned_patch_bytes","patch_truncated","warnings"]});
    if !range {
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
