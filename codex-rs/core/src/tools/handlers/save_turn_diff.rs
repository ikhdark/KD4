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
    operation: Option<u64>,
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for SaveTurnDiffHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(SAVE_TURN_DIFF_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: SAVE_TURN_DIFF_TOOL_NAME.into(),
            description: "Save this turn's exact tracked net diff to .codex/gitdiff/<name>.patch. Refuses partial diffs and occupied artifact names. After a discontinuity, select an available operation number to save its historical committed before/after receipt as <name>.operation-<number>.json instead. Operation receipts do not reconstruct an integrated diff or verify current source.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    ("environment_id".into(), JsonSchema::string(Some(
                        "Select one environment when several have tracked edits; save each separately. Replay paths are relative to that environment's working root, never UI labels.".into(),
                    ))),
                    (
                        "name".into(),
                        JsonSchema::string(Some(format!(
                            "Implementation name used as the file stem: 1-{MAX_NAME_CHARS} letters, digits, '.', '_' or '-', not starting with '.'."
                        ))),
                    ),
                    (
                        "operation".into(),
                        JsonSchema {
                            minimum: Some(1.into()),
                            ..JsonSchema::integer(Some(
                                "Optional historical operation number returned with a partial-diff refusal; omitted selects the net patch.".into(),
                            ))
                        },
                    ),
                ]),
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
            let (diff, unattributed, receipt, operations) = {
                let tracker = invocation.tracker.lock().await;
                (
                    tracker.exact_export_diff(args.environment_id.as_deref()),
                    tracker.unattributed_display_paths(),
                    args.operation
                        .and_then(|sequence| tracker.owned_operation_receipt(sequence)),
                    tracker.owned_operation_summary(),
                )
            };
            let receipt_files = receipt
                .as_ref()
                .and_then(|receipt| receipt["changes"].as_array())
                .map_or(0, Vec::len);
            let mut export_environment = args.environment_id.clone();
            let diff = if let Some(sequence) = args.operation {
                let receipt = receipt.ok_or_else(|| {
                    FunctionCallError::RespondToModel(format!(
                        "operation {sequence} is not retained; {operations}"
                    ))
                })?;
                serde_json::to_string_pretty(&receipt)
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
            } else {
                match diff {
                    Ok(Some((environment, diff))) => { export_environment = Some(environment); diff },
                    Ok(None) => {
                        return Err(FunctionCallError::RespondToModel(
                            "This turn has no tracked apply_patch edits to save.".into(),
                        ));
                    }
                    Err(paths) if paths.is_empty() => {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "The turn diff is unavailable because command effects could not be tracked. Historical operation receipts are not integrated patches: {operations}. Select operation explicitly to save a retained receipt; do not infer ownership from current reads."
                        )));
                    }
                    Err(paths) => {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "The turn diff is partial. These paths have no exact integrated diff: {}. Historical operation receipts are available separately: {operations}. Select operation explicitly; do not reconstruct ownership from current reads.",
                            paths.join(", "),
                        )));
                    }
                }
            };
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                export_environment.as_deref(),
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
                .join(&match args.operation {
                    Some(sequence) => format!("{name}.operation-{sequence}.json"),
                    None => format!("{name}.patch"),
                })
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
            match filesystem
                .mutate_file_if_matches(&path, None, Some(diff.as_bytes().to_vec()), Some(&sandbox))
                .await
                .map_err(save_error)?
            {
                codex_exec_server::FileMutationOutcome::Committed => {}
                codex_exec_server::FileMutationOutcome::Conflict => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "refusing to replace existing diff artifact {native_path}; choose a new name"
                    )));
                }
                codex_exec_server::FileMutationOutcome::RecoveryRequired {
                    retained_path,
                    message,
                } => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "saving {native_path} requires recovery: {message}; retained object: {}. Do not replay the write.",
                        retained_path.display(),
                    )));
                }
            }
            let mut result = json!({
                "path": native_path,
                "environment_id": environment.environment_id,
                "replay_root": environment.cwd().to_string(),
                "files": if args.operation.is_some() { receipt_files } else { diff.lines().filter(|line| line.starts_with("diff --git ")).count() },
                "bytes": diff.len(),
                "scope": if args.operation.is_some() { "historical_committed_operation" } else { "tracked_net_patch" },
                "current_source_verified": false,
                "coherent_snapshot_verified": false,
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
