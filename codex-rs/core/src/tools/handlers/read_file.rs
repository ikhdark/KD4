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
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::command_output_artifact::read_tool_output_selectors_with_reuse;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS;
use crate::tools::handlers::read_tool_output_spec::read_tool_output_output_schema;
use crate::tools::handlers::read_tool_output_spec::tool_output_selector_schema;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct ReadFileHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
    environment_id: Option<String>,
    selectors: Vec<ToolOutputSelector>,
}

fn file_selector_schema() -> JsonSchema {
    let mut schema = tool_output_selector_schema();
    if let Some(variants) = &mut schema.one_of {
        variants.retain(|variant| {
            variant
                .properties
                .as_ref()
                .and_then(|properties| properties.get("kind"))
                .and_then(|kind| kind.enum_values.as_ref())
                .is_some_and(|values| {
                    values
                        .iter()
                        .any(|value| matches!(value.as_str(), Some("bytes" | "lines" | "search")))
                })
        });
    }
    schema
}

impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read_file")
    }

    fn spec(&self) -> ToolSpec {
        let mut selectors = JsonSchema::array(file_selector_schema(), None);
        selectors.min_items = Some(1);
        selectors.max_items = Some(READ_TOOL_OUTPUT_MAX_SELECTORS as u64);
        let mut output = read_tool_output_output_schema(file_selector_schema());
        output["properties"]["path"] = json!({"type": "string"});
        output["required"]
            .as_array_mut()
            .expect("object schema")
            .push(json!("path"));
        ToolSpec::Function(ResponsesApiTool {
            name: "read_file".to_string(),
            description: "Read selected text from a file without shell quoting. Use lines, bytes, or fixed-string search with context, with the same selectors as read_tool_output. Reads UTF-8 files up to 8 MiB through the selected environment's filesystem permissions. Returns the resolved path, content hash, and an immutable artifact_id; use read_tool_output with that artifact_id and returned continuation or child_selectors for more of the same snapshot. A new read_file call reads current disk contents.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".to_string(), JsonSchema::string(Some("File path, relative to the environment cwd or absolute.".to_string()))),
                ("environment_id".to_string(), JsonSchema::string(Some("Environment id; omit to use the primary environment.".to_string()))),
                ("selectors".to_string(), selectors),
            ]), Some(vec!["path".to_string(), "selectors".to_string()]), Some(false.into())),
            output_schema: Some(output),
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
            if args.selectors.is_empty() || args.selectors.len() > READ_TOOL_OUTPUT_MAX_SELECTORS {
                return Err(FunctionCallError::RespondToModel(format!(
                    "read_file requires 1-{READ_TOOL_OUTPUT_MAX_SELECTORS} selectors"
                )));
            }
            if args.selectors.iter().any(|selector| {
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
            let environment = resolve_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
            )?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "read_file is unavailable without an environment".to_string(),
                )
            })?;
            let path = environment
                .cwd()
                .join(&args.path)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            let turn = &invocation.step_context.turn;
            let sandbox = turn.file_system_sandbox_context(None, environment.cwd());
            let fs = environment.environment.get_filesystem();
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
                return Err(FunctionCallError::RespondToModel(
                    "file exceeds the 8 MiB read limit".to_string(),
                ));
            }
            let contents = fs
                .read_file_bounded(&path, MAX_FILE_BYTES, Some(&sandbox))
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "unable to read {}: {err}",
                        path.inferred_native_path_string()
                    ))
                })?
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "file exceeds the 8 MiB read limit or changed while being read".to_string(),
                    )
                })?;
            let contents = String::from_utf8(contents).map_err(|_| {
                FunctionCallError::RespondToModel("read_file requires UTF-8 text".to_string())
            })?;
            if invocation.cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "read_file cancelled".to_string(),
                ));
            }
            let thread_id = invocation.session.thread_id.to_string();
            let artifact = create_canonical_output_artifact(
                &turn.config.codex_home,
                &thread_id,
                &CanonicalToolResult::text(contents),
            )
            .await;
            if !artifact.complete {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unable to retain file snapshot: {}",
                    artifact.error.unwrap_or_default()
                )));
            }
            let artifact_id = artifact
                .id
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "file snapshot has no artifact identity".to_string(),
                    )
                })?
                .to_string();
            let (result, _) = read_tool_output_selectors_with_reuse(
                &turn.config.codex_home,
                &thread_id,
                &artifact_id,
                args.selectors,
            )
            .await
            .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
            let mut output = serde_json::to_value(result)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            output["path"] = json!(path.inferred_native_path_string());
            Ok(boxed_tool_output(JsonToolOutput::new(output)))
        })
    }
}

impl CoreToolRuntime for ReadFileHandler {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::tools::context::ToolCallSource;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::models::PermissionProfile;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

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
        assert_eq!(result["canonical_sha256"].as_str().unwrap().len(), 64);
        let spec = serde_json::to_value(ReadFileHandler.spec()).unwrap();
        jsonschema::validator_for(&spec["output_schema"])
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
        assert_eq!(result["results"][0]["text"], "before");
        assert_eq!(result["results"][1]["value"]["total_matches"], 1);
        assert_eq!(
            result["results"][1]["value"]["hydrated_ranges"][0]["text"],
            "before\nneedle\nafter\n"
        );
        let spec = serde_json::to_value(ReadFileHandler.spec()).unwrap();
        jsonschema::validator_for(&spec["output_schema"])
            .unwrap()
            .validate(&result)
            .unwrap();
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
        let binary = dir.path().join("binary.dat");
        std::fs::write(&binary, [0xff, 0xfe]).unwrap();
        let oversized = dir.path().join("oversized.txt");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        for (path, expected) in [
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
