use std::collections::BTreeMap;

use codex_exec_server::CreateDirectoryOptions;
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

pub(crate) const SAVE_TURN_DIFF_TOOL_NAME: &str = "save_turn_diff";
const GITDIFF_DIRECTORY: &str = ".codex/gitdiff";
const MAX_NAME_CHARS: usize = 100;

/// Writes the turn's exact apply_patch diff, so the model never rebuilds a
/// diff the harness already tracks.
pub(crate) struct SaveTurnDiffHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveTurnDiffArgs {
    name: String,
}

impl ToolExecutor<ToolInvocation> for SaveTurnDiffHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(SAVE_TURN_DIFF_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: SAVE_TURN_DIFF_TOOL_NAME.into(),
            description: "Save this turn's net apply_patch diff, exactly as the harness tracked it, to .codex/gitdiff/<name>.patch in the primary environment and return the path. Call it once after the last edit instead of building a diff. It refuses, naming the paths, when commands or other writers changed files that have no exact diff.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "name".into(),
                    JsonSchema::string(Some(format!(
                        "Implementation name used as the file stem: 1-{MAX_NAME_CHARS} letters, digits, '.', '_' or '-', not starting with '.'."
                    ))),
                )]),
                Some(vec!["name".into()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "save_turn_diff requires function arguments".into(),
                ));
            };
            let args: SaveTurnDiffArgs = parse_arguments(arguments)?;
            let name = validated_patch_name(&args.name)?;
            let (diff, unattributed) = {
                let tracker = invocation.tracker.lock().await;
                (
                    tracker.exact_unified_diff(),
                    tracker.unattributed_display_paths(),
                )
            };
            let diff = match diff {
                Ok(Some(diff)) => diff,
                Ok(None) => {
                    return Err(FunctionCallError::RespondToModel(
                        "This turn has no tracked apply_patch edits to save.".into(),
                    ));
                }
                Err(paths) if paths.is_empty() => {
                    return Err(FunctionCallError::RespondToModel(
                        "The turn diff is unavailable because command effects could not be tracked. Build this patch from the files you changed.".into(),
                    ));
                }
                Err(paths) => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "The turn diff is partial. These paths changed outside apply_patch and have no exact diff: {}. Build this patch yourself, including only your own changes to them.",
                        paths.join(", ")
                    )));
                }
            };
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                /*environment_id*/ None,
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "save_turn_diff requires a selected execution environment".into(),
                )
            })?;
            let directory = environment
                .cwd()
                .join(GITDIFF_DIRECTORY)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let path = directory
                .join(&format!("{name}.patch"))
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let sandbox = invocation
                .step_context
                .turn
                .file_system_sandbox_context(None, environment.cwd());
            let filesystem = environment.environment.get_filesystem();
            let native_path = path.inferred_native_path_string();
            let save_error = |error: std::io::Error| {
                FunctionCallError::RespondToModel(format!("unable to save {native_path}: {error}"))
            };
            filesystem
                .create_directory(
                    &directory,
                    CreateDirectoryOptions { recursive: true },
                    Some(&sandbox),
                )
                .await
                .map_err(save_error)?;
            filesystem
                .write_file(&path, diff.as_bytes().to_vec(), Some(&sandbox))
                .await
                .map_err(save_error)?;
            let mut result = json!({
                "path": native_path,
                "files": diff.lines().filter(|line| line.starts_with("diff --git ")).count(),
                "bytes": diff.len(),
            });
            if !unattributed.is_empty() {
                // Another writer's change, or an undeclared effect of this turn's commands.
                result["unattributed_changes"] = json!(format!(
                    "Changed during this turn outside its tracked edits, not in the patch: {}. Add any your own commands wrote.",
                    crate::session::turn_execution::bounded_path_list(&unattributed)
                ));
            }
            Ok(boxed_tool_output(JsonToolOutput::new(result)))
        })
    }
}

impl CoreToolRuntime for SaveTurnDiffHandler {}

fn validated_patch_name(name: &str) -> Result<&str, FunctionCallError> {
    let valid = (1..=MAX_NAME_CHARS).contains(&name.chars().count())
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(name)
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "name must be 1-{MAX_NAME_CHARS} letters, digits, '.', '_' or '-', not starting with '.'"
        )))
    }
}

#[cfg(test)]
#[path = "save_turn_diff_tests.rs"]
mod tests;
