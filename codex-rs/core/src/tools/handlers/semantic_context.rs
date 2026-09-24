use std::collections::BTreeMap;

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;

use super::parse_arguments;
use super::resolve_tool_environment;
use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct SemanticContextHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    repository: String,
    queries: Vec<Query>,
    environment_id: Option<String>,
    configuration: Option<serde_json::Value>,
    #[serde(default)]
    compiler_check: bool,
    migration_id: Option<String>,
    #[serde(default)]
    reviewed_consumers: Vec<String>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Query {
    path: String,
    line: u32,
    column: u32,
}

impl ToolExecutor<ToolInvocation> for SemanticContextHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("semantic_context")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name:"semantic_context".into(), strict:false, defer_loading:None, output_schema:None,
            description:"Resolve 1-16 Rust source positions together through KDA's embedded Rust provider. Requires compatible cargo-kda on PATH or host CODEX_KDA_EXECUTABLE; no fallback to an external language server. KDA owns definitions, types, incoming callers, references, complete source units, revision-bound edit handles, migrations and compiler verification. Features and target configure analysis; packages restrict compiler verification, not reference coverage. Build scripts/procedural macros default off. compiler_check returns Cargo diagnostics. migration_id retains discovered consumers after removal; reviewed_consumers records reviews at current revisions. Completion requires reviewed consumers and workspace-wide compiler success; unresolved or unconfigured macro/generated consumers never establish absence. Lines and UTF-16 columns are one-based. Normal exec_command sandbox, cancellation and artifact handling apply; finish yielded processes with write_stdin. Source/configuration changes invalidate evidence. Local environments only.".into(),
            parameters:JsonSchema::object(BTreeMap::from([
                ("repository".into(),JsonSchema::string(Some("Repository root, relative to current cwd or absolute.".into()))),
                ("environment_id".into(),JsonSchema::string(None)),
                ("configuration".into(),JsonSchema::object(BTreeMap::from([
                    ("features".into(),JsonSchema::array(JsonSchema::string(None),None)),
                    ("no_default_features".into(),JsonSchema::boolean(None)),
                    ("target".into(),JsonSchema::string(None)),
                    ("packages".into(),JsonSchema::array(JsonSchema::string(None),None)),
                    ("build_scripts".into(),JsonSchema::boolean(None)),
                    ("procedural_macros".into(),JsonSchema::boolean(None)),
                ]),None,Some(false.into()))),
                ("compiler_check".into(),JsonSchema::boolean(Some("Run cargo check for the configured packages, features and target; return structured diagnostics.".into()))),
                ("migration_id".into(),JsonSchema::string(Some("Retain the complete consumer worklist under this task-local identifier. Reuse it while changing a representation.".into()))),
                ("reviewed_consumers".into(),JsonSchema::array(JsonSchema::string(Some("IDs from the retained migration whose implementations were updated or verified compatible. Completion also requires compiler_check success.".into())),None)),
                ("queries".into(),JsonSchema::array(JsonSchema::object(BTreeMap::from([
                    ("path".into(),JsonSchema::string(Some("Rust source path relative to repository.".into()))),
                    ("line".into(),JsonSchema::integer(Some("One-based line.".into()))),
                    ("column".into(),JsonSchema::integer(Some("One-based UTF-16 column.".into())))
                ]),Some(vec!["path".into(),"line".into(),"column".into()]),Some(false.into())),None))
            ]),Some(vec!["repository".into(),"queries".into()]),Some(false.into())),
        })
    }
    fn handle(&self, _: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Err(FunctionCallError::RespondToModel(
                "semantic_context must be expanded through the tool router".into(),
            ))
        })
    }
}

impl CoreToolRuntime for SemanticContextHandler {
    fn owns_unified_exec_processes(&self) -> bool {
        true
    }
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
    fn cancellation_requires_commit_barrier(&self) -> bool {
        true
    }
    fn prepare_invocation<'a>(
        &'a self,
        mut invocation: ToolInvocation,
    ) -> BoxFuture<'a, Result<ToolInvocation, FunctionCallError>> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "semantic_context requires function arguments".into(),
                ));
            };
            let args: Args = parse_arguments(arguments)?;
            if args.queries.is_empty()
                || args.queries.len() > 16
                || args.queries.iter().any(|q| q.line == 0 || q.column == 0)
            {
                return Err(FunctionCallError::RespondToModel(
                    "provide 1-16 queries with one-based lines and columns".into(),
                ));
            }
            let environment = resolve_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
            )?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel("execution environment is not ready".into())
            })?;
            if environment.environment.is_remote() {
                return Err(FunctionCallError::RespondToModel(
                    "semantic_context currently requires a local execution environment".into(),
                ));
            }
            let executable = invocation
                .step_context
                .turn
                .config
                .codex_self_exe
                .as_ref()
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "Codex worker executable is unavailable".into(),
                    )
                })?;
            let repository = environment
                .cwd()
                .join(&args.repository)
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            let mut repository_path = repository.to_path_buf();
            if let Some(transaction) = crate::workspace_transaction::load(
                &invocation.step_context.turn.config.codex_home,
                &invocation.session.thread_id.to_string(),
            )
            .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?
            .filter(|t| !t.reconciled)
            {
                repository_path =
                    crate::workspace_transaction::map_path(&transaction, &repository_path, ".")
                        .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            }
            let request = json!({"repository":repository_path,"queries":args.queries,
                "configuration":args.configuration.unwrap_or_else(||json!({})),"compiler_check":args.compiler_check,
                "migration_id":args.migration_id,"reviewed_consumers":args.reviewed_consumers,
                "state_directory":invocation.step_context.turn.config.codex_home.join("semantic-context").join(invocation.session.thread_id.to_string())});
            invocation.tool_name = ToolName::plain("exec_command");
            let mut command = json!({
                "program":executable,"args":["--codex-workspace-worker","semantic_context",request.to_string()],
                "workdir":repository_path,
                "yield_time_ms":1000,"max_output_tokens":6000
            });
            if invocation
                .step_context
                .environments
                .primary()
                .is_some_and(|primary| primary.environment_id != environment.environment_id)
            {
                command["environment_id"] = json!(environment.environment_id);
            }
            invocation.payload = ToolPayload::Function {
                arguments: command.to_string(),
            };
            Ok(invocation)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::session::Session;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::session::turn_context::TurnEnvironment;
    use crate::tools::context::ToolCallSource;
    use crate::tools::handlers::ExecCommandHandler;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::router::ToolRouter;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_utils_path_uri::PathUri;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    async fn fixture(
        root: &std::path::Path,
        arguments: serde_json::Value,
    ) -> (Arc<ToolRouter>, ToolInvocation) {
        let (session, mut turn) = make_session_and_context().await;
        Arc::make_mut(&mut turn.config).codex_self_exe =
            Some(root.parent().unwrap().join("semantic-fixture-codex.exe"));
        turn.environments.turn_environments = vec![TurnEnvironment::new(
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            Arc::new(codex_exec_server::Environment::default_for_tests()),
            PathUri::from_host_native_path(root).unwrap(),
            None,
        )];
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([
                Arc::new(SemanticContextHandler) as Arc<dyn CoreToolRuntime>,
                Arc::new(ExecCommandHandler::default()) as Arc<dyn CoreToolRuntime>,
            ]),
            Vec::new(),
        ));
        let invocation = ToolInvocation {
            session: Arc::new(session) as Arc<Session>,
            step_context: StepContext::for_test(Arc::new(turn))
                .with_tool_router_for_test(router.clone()),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "semantic-query".into(),
            tool_name: ToolName::plain("semantic_context"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: arguments.to_string(),
            },
        };
        (router, invocation)
    }

    #[tokio::test]
    async fn router_expands_semantic_query_to_registered_sandboxed_argv() {
        let dir = tempfile::tempdir().unwrap();
        let queries = json!([{"path":"src/a '$file.rs","line":4,"column":8}]);
        let (router, invocation) =
            fixture(dir.path(), json!({"repository":".","queries":queries})).await;
        let (call, notice) = router.prepare_hook_input(invocation).await.unwrap();
        assert_eq!(call.tool_name, ToolName::plain("exec_command"));
        assert!(notice.unwrap().contains("normal execution policy"));
        let ToolPayload::Function { arguments } = call.payload else {
            panic!("structured command required")
        };
        let value: serde_json::Value = serde_json::from_str(&arguments).unwrap();
        let ToolSpec::Function(spec) = ExecCommandHandler::default().spec() else {
            panic!("command schema required")
        };
        let schema = serde_json::to_value(spec.parameters).unwrap();
        jsonschema::validator_for(&schema)
            .unwrap()
            .validate(&value)
            .unwrap();
        assert!(value.get("cmd").is_none());
        assert!(value.get("sandbox_permissions").is_none());
        assert_eq!(value["args"][0], "--codex-workspace-worker");
        assert_eq!(value["args"][1], "semantic_context");
        let worker: serde_json::Value =
            serde_json::from_str(value["args"][2].as_str().unwrap()).unwrap();
        assert_eq!(worker["queries"], queries);
        assert_eq!(
            value["program"],
            json!(
                dir.path()
                    .parent()
                    .unwrap()
                    .join("semantic-fixture-codex.exe")
            )
        );
        assert!(SemanticContextHandler.owns_unified_exec_processes());
        assert!(SemanticContextHandler.waits_for_runtime_cancellation());
    }

    #[tokio::test]
    async fn isolated_queries_resolve_task_source_before_command_hooks_and_routing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.join("lib.rs"), "pub fn original() {}\n").unwrap();
        let (router, invocation) = fixture(
            &repo,
            json!({"repository":".","queries":[{"path":"lib.rs","line":1,"column":8}]}),
        )
        .await;
        let home = invocation.step_context.turn.config.codex_home.clone();
        let thread = invocation.session.thread_id.to_string();
        let tx = crate::workspace_transaction::begin(&home, &thread, &repo).unwrap();
        std::fs::write(tx.workdir.join("lib.rs"), "pub fn task_version() {}\n").unwrap();
        let (mut call, _) = router.prepare_hook_input(invocation).await.unwrap();
        let ToolPayload::Function { ref arguments } = call.payload else {
            panic!("structured command required")
        };
        let before: serde_json::Value = serde_json::from_str(arguments).unwrap();
        let worker: serde_json::Value =
            serde_json::from_str(before["args"][2].as_str().unwrap()).unwrap();
        let worker_root = std::path::Path::new(worker["repository"].as_str().unwrap());
        assert_eq!(
            std::fs::canonicalize(worker_root).unwrap(),
            std::fs::canonicalize(&tx.workdir).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(worker_root.join("lib.rs")).unwrap(),
            "pub fn task_version() {}\n"
        );
        assert!(before["environment_id"].is_null());
        crate::workspace_transaction::route_call(
            &home,
            &thread,
            &repo,
            "exec_command",
            &mut call.payload,
        )
        .unwrap();
        let ToolPayload::Function { arguments } = call.payload else {
            panic!("structured command required")
        };
        let after: serde_json::Value = serde_json::from_str(&arguments).unwrap();
        assert_eq!(after["args"], before["args"]);
        assert_eq!(
            std::fs::read_to_string(repo.join("lib.rs")).unwrap(),
            "pub fn original() {}\n"
        );
    }

    #[tokio::test]
    async fn invalid_positions_are_rejected_before_command_execution() {
        let dir = tempfile::tempdir().unwrap();
        let (router, invocation) = fixture(
            dir.path(),
            json!({"repository":".","queries":[{"path":"x.rs","line":0,"column":1}]}),
        )
        .await;
        assert!(
            router
                .prepare_hook_input(invocation)
                .await
                .unwrap_err()
                .to_string()
                .contains("one-based")
        );
    }

    #[test_case::test_case(false; "direct")]
    #[test_case::test_case(true; "code_mode")]
    #[tokio::test]
    async fn direct_registry_dispatch_selects_the_expanded_command_handler(code_mode: bool) {
        struct CaptureCommand;
        impl ToolExecutor<ToolInvocation> for CaptureCommand {
            fn tool_name(&self) -> ToolName {
                ToolName::plain("exec_command")
            }
            fn spec(&self) -> ToolSpec {
                ExecCommandHandler::default().spec()
            }
            fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
                Box::pin(async move {
                    let ToolPayload::Function { arguments } = invocation.payload else {
                        panic!("expected command")
                    };
                    let value: serde_json::Value = serde_json::from_str(&arguments).unwrap();
                    assert_eq!(value["args"][1], "semantic_context");
                    assert_eq!(invocation.tool_name, ToolName::plain("exec_command"));
                    Ok(crate::tools::context::boxed_tool_output(
                        codex_tools::JsonToolOutput::new(json!({"expanded_command_executed":true})),
                    ))
                })
            }
        }
        impl CoreToolRuntime for CaptureCommand {}
        let dir = tempfile::tempdir().unwrap();
        let (_, mut invocation) = fixture(
            dir.path(),
            json!({"repository":".","queries":[{"path":"lib.rs","line":1,"column":1}]}),
        )
        .await;
        if code_mode {
            invocation.source = ToolCallSource::CodeMode {
                cell_id: "helper-cell".into(),
                parent_call_id: None,
                runtime_tool_call_id: "helper-call".into(),
                nested_deadline: None,
                cancellation_cause: None,
            };
        }
        let registry = ToolRegistry::from_tools([
            Arc::new(SemanticContextHandler) as Arc<dyn CoreToolRuntime>,
            Arc::new(CaptureCommand) as Arc<dyn CoreToolRuntime>,
        ]);
        let state = Arc::new(crate::tools::context::ToolDispatchState::new());
        assert!(state.try_admit());
        let output = registry
            .dispatch_any_with_terminal_outcome(invocation, state)
            .await
            .unwrap();
        assert!(
            output
                .result
                .log_preview()
                .contains("expanded_command_executed")
        );
    }
}
