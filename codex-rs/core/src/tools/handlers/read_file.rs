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
use crate::tools::command_output_artifact::select_file_snapshot;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS;
use crate::tools::handlers::read_tool_output_spec::file_selector_schema;
use crate::tools::handlers::read_tool_output_spec::read_tool_output_output_schema;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const MAX_FILE_MIB: usize = 8;
const MAX_FILE_BYTES: usize = MAX_FILE_MIB * 1024 * 1024;

pub(crate) struct ReadFileHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
    environment_id: Option<String>,
    selectors: Option<Vec<ToolOutputSelector>>,
}

impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read_file")
    }

    fn spec(&self) -> ToolSpec {
        let mut selectors = JsonSchema::array(file_selector_schema(), Some("Omit to read the first page immediately. Explicit selectors are exact; oversized selections return child_selectors for the retained snapshot.".to_string()));
        selectors.min_items = Some(1);
        selectors.max_items = Some(READ_TOOL_OUTPUT_MAX_SELECTORS as u64);
        let mut output = read_tool_output_output_schema(file_selector_schema());
        output["properties"]["path"] = json!({"type": "string"});
        output["properties"]["total_lines"] = json!({"type": "integer", "minimum": 0});
        output["properties"]["artifact_id"] = json!({"type": ["string", "null"], "description": "Immutable snapshot identity when retained; null for complete inline reads or unavailable storage."});
        output["properties"]["file_complete"] = json!({"type": "boolean", "description": "The returned default page contains the entire file. Explicit selectors do not imply whole-file coverage."});
        output["properties"]["continuation"] =
            serde_json::to_value(file_selector_schema()).unwrap_or_default();
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
            description: format!("Read a UTF-8 file without shell quoting. Path alone returns useful text immediately: the whole file if it fits, otherwise its first page plus continuation. Use read_tool_output with the artifact_id and continuation for the remaining immutable snapshot. complete describes delivery of the requested page or explicit selectors; file_complete describes whole-file coverage. Explicit lines (for example start 40, end 90), bytes, or fixed-string search selectors remain exact; check each results[] status. Before editing, read the complete enclosing function, type, or configuration unit. Files may be up to {MAX_FILE_MIB} MiB. Complete inline reads need no artifact. Snapshot storage failure preserves inline evidence and reports snapshot_error without a recovery handle. Workspace results are freshness-tracked; a new read_file call reads current disk contents. Pass a skill: locator to read its SKILL.md; omit environment_id for host-owned skills."),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".to_string(), JsonSchema::string(Some("File path, relative to the environment cwd or absolute, or a `skill:` locator from the skills catalog.".to_string()))),
                ("environment_id".to_string(), JsonSchema::string(Some("Environment id; omit to use the primary environment.".to_string()))),
                ("selectors".to_string(), selectors),
            ]), Some(vec!["path".to_string()]), Some(false.into())),
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
            let (contents, resolved_path) = if args
                .path
                .starts_with(codex_core_skills::SKILL_CATALOG_LOCATOR_PREFIX)
            {
                read_skill_locator(&invocation, &args).await?
            } else {
                read_environment_file(&invocation, &args).await?
            };
            if invocation.cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "read_file cancelled".to_string(),
                ));
            }
            let thread_id = invocation.session.thread_id.to_string();
            let total_lines = contents.lines().count();
            let explicit_selection = args.selectors.is_some();
            let (canonical, mut result, mut continuation) =
                tokio::task::spawn_blocking(move || {
                    let canonical = CanonicalToolResult::text(contents);
                    select_file_snapshot(&canonical, args.selectors)
                        .map(|(result, continuation)| (canonical, result, continuation))
                })
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "file selection worker failed: {err}"
                    ))
                })?
                .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
            let file_complete = !explicit_selection && continuation.is_none();
            // Explicit selections retain the existing immutable-snapshot contract.
            // Complete default reads stay inline; omitted bytes need one durable
            // snapshot, but selecting them never rereads the file just written.
            let artifact = if explicit_selection || continuation.is_some() {
                Some(
                    create_canonical_output_artifact(
                        &turn.config.codex_home,
                        &thread_id,
                        &canonical,
                    )
                    .await,
                )
            } else {
                None
            };
            let artifact_id = artifact
                .as_ref()
                .filter(|artifact| artifact.complete)
                .and_then(|artifact| artifact.artifact_id());
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
            let mut output = serde_json::to_value(result)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            output["artifact_id"] = json!(artifact_id);
            output["retained_artifact_complete"] = json!(artifact_id.is_some());
            output["path"] = json!(resolved_path);
            output["total_lines"] = json!(total_lines);
            output["file_complete"] = json!(file_complete);
            if let Some(continuation) = continuation {
                output["continuation"] = json!(continuation);
            }
            if let Some(error) = snapshot_error {
                output["snapshot_error"] = json!(error);
            }
            Ok(boxed_tool_output(JsonToolOutput::new(output)))
        })
    }
}

/// Reads an ordinary path through the selected environment's filesystem.
async fn read_environment_file(
    invocation: &ToolInvocation,
    args: &ReadFileArgs,
) -> Result<(String, String), FunctionCallError> {
    let environment = resolve_tool_environment(
        &invocation.step_context.environments,
        args.environment_id.as_deref(),
    )?
    .ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "read_file requires a ready execution environment for filesystem paths. If an environment is starting, call wait_for_environment with its id first.".to_string(),
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
        return Err(FunctionCallError::RespondToModel(format!(
            "file exceeds the {MAX_FILE_MIB} MiB read limit"
        )));
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
            FunctionCallError::RespondToModel(format!(
                "file exceeds the {MAX_FILE_MIB} MiB read limit or changed while being read"
            ))
        })?;
    let contents = String::from_utf8(contents).map_err(|_| {
        FunctionCallError::RespondToModel("read_file requires UTF-8 text".to_string())
    })?;
    Ok((contents, path.inferred_native_path_string()))
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

impl CoreToolRuntime for ReadFileHandler {}

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
                .project_with_workspace_identity(canonical.clone(), revision.as_ref())
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
        let notice: serde_json::Value = serde_json::from_str(&output.body.to_text().unwrap()).unwrap();
        assert_eq!(notice["reason_code"], "source_dependencies_invalidated");
        assert_eq!(notice["rerun"]["tool"], "read_file");
        let retry = notice["rerun"]["arguments"].clone();
        assert_eq!(retry, json!({"path": "file.txt"}));
        let ToolSpec::Function(spec) = ReadFileHandler.spec() else {
            panic!("expected callable read schema");
        };
        let parameters = serde_json::to_value(spec.parameters).unwrap();
        assert!(jsonschema::validator_for(&parameters).unwrap().is_valid(&retry));
        let recovered = runtime.handle_tool_call_with_source(
            ToolCall {
                tool_name: ToolName::plain(notice["rerun"]["tool"].as_str().unwrap()),
                call_id: "recovered-file-read".into(),
                payload: ToolPayload::Function { arguments: retry.to_string() },
            },
            ToolCallSource::Direct,
            CancellationToken::new(),
        ).await.unwrap().code_mode_result();
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
            jsonschema::validator_for(spec.output_schema.as_ref().unwrap())
                .unwrap()
                .validate(&result)
                .unwrap();
        }
        assert!(!validator.is_valid(&json!({"path": path, "selectors": []})));
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
            jsonschema::validator_for(spec.output_schema.as_ref().unwrap())
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
        jsonschema::validator_for(spec.output_schema.as_ref().unwrap())
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
        jsonschema::validator_for(spec.output_schema.as_ref().unwrap())
            .unwrap()
            .validate(&result)
            .unwrap();
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
        jsonschema::validator_for(spec.output_schema.as_ref().unwrap())
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
            message.contains("read_file requires a ready execution environment")
                && message.contains("wait_for_environment"),
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
