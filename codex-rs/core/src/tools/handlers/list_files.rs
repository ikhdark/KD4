use std::collections::BTreeMap;

use codex_exec_server::WalkFilters;
use codex_exec_server::WalkOptions;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathConvention;
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

pub(crate) struct ListFilesHandler;

const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 50_000;

fn render_listing_path(cwd: &str, native: String, convention: Option<PathConvention>) -> String {
    let Some(convention) = convention else {
        return native;
    };
    let windows = convention == PathConvention::Windows;
    let normalized_cwd = windows.then(|| cwd.replace('\\', "/"));
    let cwd = normalized_cwd.as_deref().unwrap_or(cwd);
    let normalized = windows.then(|| native.replace('\\', "/"));
    let path = normalized.as_deref().unwrap_or(&native);
    match path.strip_prefix(cwd) {
        Some("") => ".".to_string(),
        Some(rest) if cwd.ends_with('/') => rest.to_string(),
        Some(rest) if rest.starts_with('/') => rest[1..].to_string(),
        _ => native,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListFilesArgs {
    path: String,
    environment_id: Option<String>,
    max_depth: Option<usize>,
    max_entries: Option<usize>,
    max_directories: Option<usize>,
    include_hidden: Option<bool>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    exclude_directories: Vec<String>,
}

impl ToolExecutor<ToolInvocation> for ListFilesHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_files")
    }

    fn spec(&self) -> ToolSpec {
        let mut depth = JsonSchema::integer(Some(
            "Maximum descendant depth, 0 for an immediate directory listing; default 8, clamped to 64 (returned in effective_walk_options).".into(),
        ));
        depth.minimum = Some(0.into());
        let mut entries = JsonSchema::integer(Some(
            "Maximum entries examined, including directories; with include patterns, maximum matching files returned instead while up to 50000 entries are examined. Default 2000, clamped to 50000 (returned in effective_walk_options).".into(),
        ));
        entries.minimum = Some(1.into());
        ToolSpec::Function(ResponsesApiTool {
            name: "list_files".into(),
            description: "List files and directories through the selected environment's sandboxed filesystem without shell quoting. Start at a narrow path. For the same root and walk options, combine independently known include patterns in one call instead of repeating the walk; limits apply to the combined results, so check completeness. File include/exclude filters do not prune directories; with include patterns, entries are only matching files and directories are traversed without being listed. exclude_directories omits and prunes matching directories. Patterns match case-sensitive basenames at every depth: * is zero or more Unicode characters, ? is one; other characters are literal. At most 64 patterns total, each 1–256 UTF-8 bytes without path separators. Entry paths are relative to the environment cwd (absolute when outside it) and can be passed to read_file or list_files. Directory symlinks and dangling links are omitted from entries and never followed; hidden directories are listed (except with include patterns) but not traversed unless include_hidden is true. For an unfiltered audit omit all filters and set include_hidden=true; gitignore is never applied. complete refers only to this requested scope. Check truncated, errors, unexplored cutoff reasons, and effective_walk_options before claiming coverage. Directories are mutable, not stable pages. Recover large output with read_tool_output; repeat list_files for current state.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Directory path, relative to the environment cwd or absolute.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Omit to use the primary environment.".into()))),
                ("max_depth".into(), depth),
                ("max_entries".into(), entries),
                ("max_directories".into(), JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some("Maximum directories traversed, including the root. Default min(max_entries,10000), or 10000 with include patterns; clamped to 10000.".into())) }),
                ("include_hidden".into(), JsonSchema::boolean(Some("Traverse hidden directories; default false.".into()))),
                ("include".into(), JsonSchema::array(JsonSchema::string(None), Some("File basename patterns; empty or omitted includes all files. When set, only matching files are listed; directories are still traversed.".into()))),
                ("exclude".into(), JsonSchema::array(JsonSchema::string(None), Some("File basename patterns omitted from results; exclusion wins over inclusion.".into()))),
                ("exclude_directories".into(), JsonSchema::array(JsonSchema::string(None), Some("Directory basename patterns omitted and never traversed, e.g. [\"node_modules\",\"target\"]. The explicitly selected root is never excluded.".into()))),
            ]), Some(vec!["path".into()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "list_files requires function arguments".into(),
                ));
            };
            let args: ListFilesArgs = parse_arguments(arguments)?;
            let max_depth = args.max_depth.unwrap_or(8).min(MAX_DEPTH);
            let max_entries = args.max_entries.unwrap_or(2000).min(MAX_ENTRIES);
            // Unfiltered walks list every traversed directory as an entry. Include
            // patterns list only matching files, so entries do not bound traversal.
            let max_directories = args
                .max_directories
                .unwrap_or(if args.include.is_empty() {
                    max_entries
                } else {
                    10_000
                })
                .min(10_000);
            let filters = WalkFilters {
                include: args.include,
                exclude: args.exclude,
                exclude_directories: args.exclude_directories,
            };
            filters
                .validate()
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            if max_entries == 0 || max_directories == 0 {
                return Err(FunctionCallError::RespondToModel(format!(
                    "list_files requires max_depth 0-{MAX_DEPTH}, max_entries 1-{MAX_ENTRIES}, and max_directories 1-10000"
                )));
            }
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "list_files requires a selected execution environment".into(),
                )
            })?;
            let path = environment
                .cwd()
                .join(&args.path)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let sandbox = invocation
                .step_context
                .turn
                .file_system_sandbox_context(None, environment.cwd());
            let filesystem = environment.environment.get_filesystem();
            let options = WalkOptions {
                max_depth,
                max_directories,
                max_entries,
                follow_directory_symlinks: false,
                prune_hidden_directories: !args.include_hidden.unwrap_or(false),
                filters,
            };
            let outcome = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => {
                    return Err(FunctionCallError::RespondToModel("list_files cancelled".into()));
                }
                result = filesystem.walk(&path, options.clone(), Some(&sandbox)) => result
                    .map_err(|error| FunctionCallError::RespondToModel(format!("unable to list {}: {error}", path.inferred_native_path_string())))?,
            };
            // read_file and list_files take native paths, not URIs. The
            // cwd-relative spelling also keeps every entry from repeating the root.
            let cwd = environment.cwd().inferred_native_path_string();
            let convention = environment.cwd().infer_path_convention();
            let display_path = |native| render_listing_path(&cwd, native, convention);
            let mut errors = outcome.errors.iter().map(|error| json!({
                "path": display_path(error.path.inferred_native_path_string()), "message": error.message,
            })).collect::<Vec<_>>();
            let entries = outcome
                .entries
                .iter()
                .filter_map(|entry| {
                    // The filesystem can preserve opaque native identities, but this
                    // tool publishes strings accepted by read_file/list_files. A lossy
                    // replacement label must never name a different healthy sibling.
                    let native = entry.path.infer_path_convention().and_then(|convention| {
                        LegacyAppPathString::from_path_uri(&entry.path, convention)
                            .ok()
                            .filter(|native| {
                                native
                                    .to_path_uri(convention)
                                    .is_ok_and(|roundtrip| roundtrip == entry.path)
                            })
                    });
                    let Some(native) = native else {
                        errors.push(json!({
                        "path": display_path(path.inferred_native_path_string()),
                        "message": "unrepresentable native entry cannot be addressed losslessly",
                    }));
                        return None;
                    };
                    Some(json!({"path": display_path(native.into_string()), "kind": entry.kind}))
                })
                .collect::<Vec<_>>();
            let complete = !outcome.truncated && errors.is_empty();
            let value = json!({
                "path": path.inferred_native_path_string(),
                "environment_id": environment.environment_id,
                "effective_walk_options": {
                    "max_depth": options.max_depth,
                    "max_entries": options.max_entries,
                    "max_directories": options.max_directories,
                    "follow_directory_symlinks": options.follow_directory_symlinks,
                    "prune_hidden_directories": options.prune_hidden_directories,
                    "filters": options.filters,
                    "max_response_bytes": codex_exec_server::MAX_WALK_RESPONSE_BYTES,
                    "apply_gitignore": false,
                },
                "limits_clamped": args.max_depth.is_some_and(|value| value > MAX_DEPTH)
                    || args.max_entries.is_some_and(|value| value > MAX_ENTRIES)
                    || args.max_directories.is_some_and(|value| value > 10_000),
                "entries": entries,
                "errors": errors,
                "truncated": outcome.truncated,
                "unexplored": outcome.unexplored.iter().map(|stop| json!({
                    "path": display_path(stop.path.inferred_native_path_string()), "reason": stop.reason,
                    "effectiveLimit": stop.effective_limit, "partiallyExamined": stop.partially_examined,
                })).collect::<Vec<_>>(),
                "continuation_policy": "Re-examine unexplored paths with adjusted limits; directories are mutable, not stable resumable pages.",
                "complete": complete,
            });
            let evidence = crate::tools::context::semantic_evidence_sampling_signal(json!({
                "source": "list_files",
                "scope": {"path": value["path"], "environment_id": value["environment_id"],
                    "walk_options": value["effective_walk_options"]},
                "identity": crate::tool_history::sha256(value.to_string().as_bytes()),
            }));
            Ok(boxed_tool_output(
                JsonToolOutput::new(value).with_sampling_request_signal(evidence),
            ))
        })
    }
}

impl CoreToolRuntime for ListFilesHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }

    fn permits_shared_workspace_observation(&self, _payload: &ToolPayload) -> bool {
        true
    }
}

