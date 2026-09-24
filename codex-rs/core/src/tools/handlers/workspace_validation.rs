use super::parse_arguments;
use super::resolve_tool_environment;
use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;
use sha2::Digest;
use std::collections::BTreeMap;

pub(crate) struct WorkspaceValidationHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    repository: String,
    action: String,
    #[serde(default)]
    checks: Vec<Check>,
    #[serde(default)]
    allow_full_suite: bool,
    #[serde(default)]
    force_fresh: bool,
    environment_id: Option<String>,
}
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Check {
    id: String,
    args: Vec<String>,
}

impl ToolExecutor<ToolInvocation> for WorkspaceValidationHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("workspace_validation")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name:"workspace_validation".into(), strict:false, defer_loading:None, output_schema:None,
            description:"Plan or run focused Cargo validation. plan maps Git changes to affected packages and test targets without executing checks. run accepts cargo check/test/clippy argv arrays, runs every selected check to completion, and returns structured compiler diagnostics, failed/passed test names, assertions and retained raw logs. Match checks to the requested behavior and required repository checks. Broad test suites require allow_full_suite and must respect user constraints. Passing checks are reused only when dependency contents, configuration, environment and toolchain fingerprints match; force_fresh overrides reuse. Output directories are warm and exclusively leased through process completion. Active workspace transactions supply immutable validation snapshots. Use write_stdin for yielded processes. Local environments only.".into(),
            parameters:JsonSchema::object(BTreeMap::from([
                ("repository".into(),JsonSchema::string(None)),
                ("action".into(),JsonSchema::string(Some("plan or run".into()))),
                ("checks".into(),JsonSchema::array(JsonSchema::object(BTreeMap::from([
                    ("id".into(),JsonSchema::string(None)),
                    ("args".into(),JsonSchema::array(JsonSchema::string(None),None)),
                ]),Some(vec!["id".into(),"args".into()]),Some(false.into())),None)),
                ("allow_full_suite".into(),JsonSchema::boolean(Some("Default false. Enable only when broad test execution is within the user's task.".into()))),
                ("force_fresh".into(),JsonSchema::boolean(None)),
                ("environment_id".into(),JsonSchema::string(None)),
            ]),Some(vec!["repository".into(),"action".into()]),Some(false.into())),
        })
    }
    fn handle(&self, _: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Err(FunctionCallError::RespondToModel(
                "workspace_validation requires router expansion".into(),
            ))
        })
    }
}

impl CoreToolRuntime for WorkspaceValidationHandler {
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
                    "workspace_validation requires function arguments".into(),
                ));
            };
            let args: Args = parse_arguments(arguments)?;
            if !matches!(args.action.as_str(), "plan" | "run") {
                return Err(FunctionCallError::RespondToModel(
                    "action must be plan or run".into(),
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
                    "workspace_validation requires a local environment".into(),
                ));
            }
            let config = &invocation.step_context.turn.config;
            let executable = config.codex_self_exe.as_ref().ok_or_else(|| {
                FunctionCallError::RespondToModel("Codex worker executable is unavailable".into())
            })?;
            let repository = environment
                .cwd()
                .join(&args.repository)
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            let mut root = repository.to_path_buf();
            let mut snapshot_root = None;
            let home = config.codex_home.clone();
            let thread = invocation.session.thread_id.to_string();
            let active = crate::workspace_transaction::load(&home, &thread)
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            let cache_identity = format!(
                "{:x}",
                sha2::Sha256::digest(
                    active
                        .as_ref()
                        .map_or_else(|| root.clone(), |t| t.origin.clone())
                        .to_string_lossy()
                        .as_bytes()
                )
            );
            if let Some(transaction) = active.filter(|t| !t.reconciled) {
                if !matches!(
                    invocation.step_context.turn.sandbox_policy(),
                    codex_protocol::protocol::SandboxPolicy::DangerFullAccess
                ) || args
                    .environment_id
                    .as_deref()
                    .is_some_and(|id| id != codex_exec_server::LOCAL_ENVIRONMENT_ID)
                {
                    return Err(FunctionCallError::RespondToModel("active workspace validation requires the local unrestricted transaction environment".into()));
                }
                let mapped = crate::workspace_transaction::map_path(&transaction, &root, ".")
                    .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
                if args.action == "run" {
                    let tail = mapped
                        .strip_prefix(&transaction.workdir)
                        .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?
                        .to_path_buf();
                    let snapshot = tokio::task::spawn_blocking(move || {
                        crate::workspace_transaction::validation_snapshot(&home, &thread)
                    })
                    .await
                    .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?
                    .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
                    root = snapshot.workdir.join(tail);
                    snapshot_root = Some(snapshot.workdir);
                } else {
                    root = mapped;
                }
            }
            let request = json!({"repository":root,"cache_identity":cache_identity,"cache_directory":config.codex_home.join("validation-cache"),
                "action":args.action,"checks":args.checks,"allow_full_suite":args.allow_full_suite,"force_fresh":args.force_fresh,
                "snapshot_root":snapshot_root});
            invocation.tool_name = ToolName::plain("exec_command");
            let mut command = json!({
                "program":executable,"args":["--codex-workspace-worker","workspace_validation",request.to_string()],
                "workdir":root,"yield_time_ms":1000,"max_output_tokens":12000,
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

    #[tokio::test]
    async fn validation_router_preserves_contract_and_captures_task_source() {
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
        std::fs::write(repo.join("lib.rs"), "original\n").unwrap();
        let (session, mut turn) = make_session_and_context().await;
        Arc::make_mut(&mut turn.config).codex_self_exe = Some(dir.path().join("codex.exe"));
        turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        turn.environments.turn_environments = vec![TurnEnvironment::new(
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            Arc::new(codex_exec_server::Environment::default_for_tests()),
            PathUri::from_host_native_path(&repo).unwrap(),
            None,
        )];
        let home = turn.config.codex_home.clone();
        let thread = session.thread_id.to_string();
        let tx = crate::workspace_transaction::begin(&home, &thread, &repo).unwrap();
        std::fs::write(tx.workdir.join("lib.rs"), "task source\n").unwrap();
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([
                Arc::new(WorkspaceValidationHandler) as Arc<dyn CoreToolRuntime>,
                Arc::new(ExecCommandHandler::default()) as Arc<dyn CoreToolRuntime>,
            ]),
            Vec::new(),
        ));
        let checks =
            json!([{"id":"focused","args":["test","-p","example","--lib","specific_test"]}]);
        let invocation = ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::new(turn))
                .with_tool_router_for_test(router.clone()),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "validation".into(),
            tool_name: ToolName::plain("workspace_validation"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments:
                    json!({"repository":".","action":"run","checks":checks,"force_fresh":true})
                        .to_string(),
            },
        };
        let (mut expanded, _) = router.prepare_hook_input(invocation).await.unwrap();
        assert_eq!(expanded.tool_name, ToolName::plain("exec_command"));
        let ToolPayload::Function { arguments } = &expanded.payload else {
            panic!("argv required")
        };
        let command: serde_json::Value = serde_json::from_str(arguments).unwrap();
        let worker: serde_json::Value =
            serde_json::from_str(command["args"][2].as_str().unwrap()).unwrap();
        let ToolSpec::Function(spec) = ExecCommandHandler::default().spec() else {
            panic!("command schema required")
        };
        let schema = serde_json::to_value(spec.parameters).unwrap();
        jsonschema::validator_for(&schema)
            .unwrap()
            .validate(&command)
            .unwrap();
        assert_eq!(command["args"][1], "workspace_validation");
        assert_eq!(worker["checks"], checks);
        assert_eq!(worker["force_fresh"], true);
        assert_eq!(worker["allow_full_suite"], false);
        let captured = std::path::Path::new(worker["repository"].as_str().unwrap());
        // The worker refreshes the captured copy after leasing its build lane.
        assert_eq!(
            std::path::Path::new(worker["snapshot_root"].as_str().unwrap()),
            captured
        );
        assert_ne!(
            std::fs::canonicalize(captured).unwrap(),
            std::fs::canonicalize(&tx.workdir).unwrap()
        );
        std::fs::write(tx.workdir.join("lib.rs"), "later change\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(captured.join("lib.rs")).unwrap(),
            "task source\n"
        );
        crate::workspace_transaction::route_call(
            &home,
            &thread,
            &repo,
            "exec_command",
            &mut expanded.payload,
        )
        .unwrap();
        let ToolPayload::Function { arguments } = &expanded.payload else {
            panic!("argv required")
        };
        let routed: serde_json::Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(routed["args"], command["args"]);
        assert_eq!(
            std::fs::read_to_string(repo.join("lib.rs")).unwrap(),
            "original\n"
        );
    }
}
