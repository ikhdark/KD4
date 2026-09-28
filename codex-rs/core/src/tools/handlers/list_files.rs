use std::collections::BTreeMap;

use codex_exec_server::WalkOptions;
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
use crate::tools::context::semantic_evidence_sampling_signal;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
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
    include_hidden: Option<bool>,
}

impl ToolExecutor<ToolInvocation> for ListFilesHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_files")
    }

    fn spec(&self) -> ToolSpec {
        let mut depth = JsonSchema::integer(Some(
            "Maximum descendant depth, 0 for an immediate directory listing; default 8.".into(),
        ));
        depth.minimum = Some(0.into());
        depth.maximum = Some((MAX_DEPTH as u64).into());
        let mut entries = JsonSchema::integer(Some(
            "Maximum entries examined, including directories; default 2000.".into(),
        ));
        entries.minimum = Some(1.into());
        entries.maximum = Some((MAX_ENTRIES as u64).into());
        ToolSpec::Function(ResponsesApiTool {
            name: "list_files".into(),
            description: "List files and directories through the selected environment's sandboxed filesystem without shell quoting. Start at a narrow path. Directory symlinks are not followed; hidden directories are listed but not traversed unless include_hidden is true. This does not apply gitignore rules. Check complete, truncated, and errors before claiming coverage. A bounded walk is a partial observation, not a stable page: narrow path or increase limits if truncated. Large output can be recovered with read_tool_output; repeat list_files for current filesystem state.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Directory path, relative to the environment cwd or absolute.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Omit to use the primary environment.".into()))),
                ("max_depth".into(), depth),
                ("max_entries".into(), entries),
                ("include_hidden".into(), JsonSchema::boolean(Some("Traverse hidden directories; default false.".into()))),
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
            let max_depth = args.max_depth.unwrap_or(8);
            let max_entries = args.max_entries.unwrap_or(2000);
            if max_depth > MAX_DEPTH || !(1..=MAX_ENTRIES).contains(&max_entries) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "list_files requires max_depth 0-{MAX_DEPTH} and max_entries 1-{MAX_ENTRIES}"
                )));
            }
            let environment = resolve_tool_environment(&invocation.step_context.environments, args.environment_id.as_deref())?
                .ok_or_else(|| FunctionCallError::RespondToModel("list_files requires a ready execution environment; use wait_for_environment if one is starting".into()))?;
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
                max_directories: max_entries.min(10_000),
                max_entries,
                follow_directory_symlinks: false,
                prune_hidden_directories: !args.include_hidden.unwrap_or(false),
            };
            let scope = json!({"environment_id": environment.environment_id, "options": options});
            let outcome = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => {
                    return Err(FunctionCallError::RespondToModel("list_files cancelled".into()));
                }
                result = filesystem.walk(&path, options, Some(&sandbox)) => result
                    .map_err(|error| FunctionCallError::RespondToModel(format!("unable to list {}: {error}", path.inferred_native_path_string())))?,
            };
            let signal =
                listing_sampling_signal(&path.inferred_native_path_string(), scope, &outcome);
            let mut output = JsonToolOutput::new(json!({
                "path": path.inferred_native_path_string(),
                "environment_id": environment.environment_id,
                "entries": outcome.entries,
                "errors": outcome.errors,
                "truncated": outcome.truncated,
                "complete": !outcome.truncated && outcome.errors.is_empty(),
            }));
            if let Some(signal) = signal {
                output = output.with_sampling_request_signal(signal);
            }
            Ok(boxed_tool_output(output))
        })
    }
}

fn listing_sampling_signal(
    path: &str,
    scope: serde_json::Value,
    outcome: &codex_file_system::WalkOutcome,
) -> Option<serde_json::Value> {
    if outcome.entries.is_empty() && (outcome.truncated || !outcome.errors.is_empty()) {
        return None;
    }
    // Enumeration order is not new evidence; completeness and errors still are.
    let mut entries = outcome
        .entries
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let mut errors = outcome
        .errors
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    entries.sort_unstable();
    errors.sort_unstable();
    let identity = crate::tool_history::sha256(
        &serde_json::to_vec(&(entries, errors, outcome.truncated)).ok()?,
    );
    Some(semantic_evidence_sampling_signal(json!({
        "source": path, "scope": scope, "identity": identity,
    })))
}

impl CoreToolRuntime for ListFilesHandler {}

#[cfg(test)]
#[path = "list_files_tests.rs"]
mod tests;
