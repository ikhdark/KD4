use std::collections::BTreeMap;

use codex_exec_server::WalkOptions;
use codex_exec_server::WalkFilters;
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

pub(crate) struct ListFilesHandler;

const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 50_000;

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
            "Maximum entries examined, including directories; default 2000, clamped to 50000 (returned in effective_walk_options).".into(),
        ));
        entries.minimum = Some(1.into());
        ToolSpec::Function(ResponsesApiTool {
            name: "list_files".into(),
            description: "List files and directories through the selected environment's sandboxed filesystem without shell quoting. Start at a narrow path. File include/exclude filters do not prune directories; exclude_directories omits and prunes matching directories. Patterns match case-sensitive basenames at every depth: * is zero or more Unicode characters, ? is one; other characters are literal. At most 64 patterns total, each 1–256 UTF-8 bytes without path separators. Directory symlinks are not followed; hidden directories are listed but not traversed unless include_hidden is true. For an unfiltered audit omit all filters and set include_hidden=true; gitignore is never applied. complete refers only to this requested scope. Check truncated, errors, unexplored cutoff reasons, and effective_walk_options before claiming coverage. Directories are mutable, not stable pages. Recover large output with read_tool_output; repeat list_files for current state.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Directory path, relative to the environment cwd or absolute.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Omit to use the primary environment.".into()))),
                ("max_depth".into(), depth),
                ("max_entries".into(), entries),
                ("max_directories".into(), JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some("Maximum directories traversed, including the root. Default min(max_entries,10000), clamped to 10000.".into())) }),
                ("include_hidden".into(), JsonSchema::boolean(Some("Traverse hidden directories; default false.".into()))),
                ("include".into(), JsonSchema::array(JsonSchema::string(None), Some("File basename patterns; empty or omitted includes all files. Directories remain visible and traversable.".into()))),
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
            let max_directories = args.max_directories.unwrap_or(max_entries).min(10_000);
            let filters = WalkFilters { include: args.include, exclude: args.exclude, exclude_directories: args.exclude_directories };
            filters.validate().map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            if max_entries == 0 || max_directories == 0 {
                return Err(FunctionCallError::RespondToModel(format!(
                    "list_files requires max_depth 0-{MAX_DEPTH}, max_entries 1-{MAX_ENTRIES}, and max_directories 1-10000"
                )));
            }
            let environment = wait_for_tool_environment(&invocation.step_context.environments, args.environment_id.as_deref(), &invocation.cancellation_token).await?
                .ok_or_else(|| FunctionCallError::RespondToModel("list_files requires a selected execution environment".into()))?;
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
                "entries": outcome.entries,
                "errors": outcome.errors,
                "truncated": outcome.truncated,
                "unexplored": outcome.unexplored,
                "continuation_policy": "Re-examine unexplored paths with adjusted limits; directories are mutable, not stable resumable pages.",
                "complete": !outcome.truncated && outcome.errors.is_empty(),
            });
            let evidence = crate::tools::context::semantic_evidence_sampling_signal(json!({
                "source": "list_files",
                "scope": {"path": value["path"], "environment_id": value["environment_id"],
                    "walk_options": value["effective_walk_options"]},
                "identity": crate::tool_history::sha256(value.to_string().as_bytes()),
            }));
            Ok(boxed_tool_output(JsonToolOutput::new(value).with_sampling_request_signal(evidence)))
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

#[cfg(test)]
#[path = "list_files_tests.rs"]
mod tests;
