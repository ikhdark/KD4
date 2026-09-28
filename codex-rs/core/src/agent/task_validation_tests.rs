use super::TaskValidation;
use codex_agent_task_store::*;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::ExecCommandHandler;
use crate::tools::handlers::ShellCommandHandler;
use crate::tools::handlers::ShellCommandHandlerOptions;
use crate::tools::registry::ToolExecutor;

struct Fixture {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    store: Arc<LocalAgentTaskStore>,
    assignment: Assignment,
    attempt: Attempt,
}

impl Fixture {
    async fn new(commands: Vec<String>) -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("input.txt"), "stable").unwrap();
        let (session, mut turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test key"),
                Vec::new(),
                home.path(),
                |config| {
                    config.permissions.approval_policy = crate::config::Constrained::allow_any(
                        codex_protocol::protocol::AskForApproval::Never,
                    );
                    config
                        .permissions
                        .set_permission_profile(codex_protocol::models::PermissionProfile::Disabled)
                        .unwrap();
                },
            )
            .await;
        let coordinator = session.services.agent_control.task_coordinator();
        coordinator
            .initialize(
                codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".into())
                    .await
                    .unwrap(),
                "validation-root".into(),
            )
            .await
            .unwrap();
        let (assignment, attempt) = coordinator
            .create_assignment(
                repo.path(),
                AssignmentDraft {
                    root_session_id: "validation-root".into(),
                    admission_origin: AssignmentAdmissionOrigin::Typed,
                    role: AgentRole::Worker,
                    capability_profile: CapabilityProfile::ScopedSourceWrite,
                    objective: "validate actual command completion".into(),
                    acceptance_criteria: vec![AcceptanceCriterion {
                        id: "execution".into(),
                        text: "command succeeds".into(),
                    }],
                    read_scope: Vec::new(),
                    write_scope: vec![RepoScope {
                        path: ".".into(),
                        recursive: true,
                    }],
                    stop_condition: "validated".into(),
                    dependencies: Vec::new(),
                    risk_hints: Vec::new(),
                    required_evidence: commands,
                    prohibited_changes: Vec::new(),
                    contract_claims: Vec::new(),
                    workspace_strategy: WorkspaceStrategy::Auto,
                    relation: None,
                    architecture_contract_ref: None,
                },
            )
            .await
            .unwrap();
        let path = AgentPath::try_from("/root/validation_worker").unwrap();
        let binding = coordinator
            .bind_agent_task(AgentTaskBindingDraft {
                assignment_id: assignment.assignment_id,
                attempt_id: attempt.attempt_id,
                agent_path: path.to_string(),
                task_name: "validation_worker".into(),
                thread_id: Some(session.thread_id.to_string()),
            })
            .await
            .unwrap();
        assert!(
            coordinator
                .heartbeat_typed_actor_binding(&binding, false)
                .await
                .unwrap()
        );
        let turn_mut = Arc::get_mut(&mut turn).unwrap();
        turn_mut.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: Some(path),
            agent_nickname: None,
            agent_role: Some("worker".into()),
        });
        let environment = turn_mut.environments.primary().unwrap().clone();
        turn_mut.environments.turn_environments = vec![TurnEnvironment::new(
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            environment.environment,
            codex_utils_path_uri::PathUri::from_abs_path(
                &codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(repo.path())
                    .unwrap(),
            ),
            environment.shell,
        )];
        let store = coordinator.store().unwrap();
        Self {
            _home: home,
            repo,
            session,
            turn,
            store,
            assignment,
            attempt,
        }
    }

    fn invocation(&self, name: &str, call_id: &str, args: serde_json::Value) -> ToolInvocation {
        ToolInvocation {
            session: Arc::clone(&self.session),
            step_context: StepContext::for_test(Arc::clone(&self.turn)),
            tracker: Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
            call_id: call_id.into(),
            tool_name: codex_tools::ToolName::plain(name),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: args.to_string(),
            },
            cancellation_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    async fn command(&self, unified: bool, id: &str, command: &str) {
        let mut args =
            json!({"script_body": command, "validation": {"covered_paths": ["input.txt"]}});
        let output = if unified {
            args["yield_time_ms"] = json!(250);
            ExecCommandHandler::default()
                .handle_call(self.invocation("exec_command", id, args))
                .await
        } else {
            ShellCommandHandler::new(ShellCommandHandlerOptions {
                foreign_environment: false,
                allow_login_shell: false,
                allow_escalated_sandbox_permissions: false,
                exec_permission_approvals_enabled: false,
            })
            .handle_call(self.invocation("shell_command", id, args))
            .await
        }
        .unwrap();
        let payload = ToolPayload::Function {
            arguments: "{}".into(),
        };
        let value = output.code_mode_result(&payload);
        if let Some(id) = value["session_id"].as_u64() {
            let output = crate::tools::handlers::WriteStdinHandler::default()
                .handle(self.invocation(
                    "write_stdin",
                    "poll",
                    json!({"session_id": id, "chars": "", "yield_time_ms": 10000}),
                ))
                .await
                .unwrap();
            assert!(
                output.code_mode_result(&payload)["session_id"].is_null(),
                "test command must settle"
            );
        }
    }
}

#[tokio::test]
async fn actual_shell_and_unified_commands_supply_receipt_evidence() {
    for unified in [false, true] {
        let command = if unified {
            "Start-Sleep -Milliseconds 700; Write-Output 'validated'; exit 0"
        } else {
            "Write-Output 'validated'; exit 0"
        };
        let fixture = Fixture::new(vec![command.into()]).await;
        fixture.command(unified, "actual-proof", command).await;
        let call = fixture
            .store
            .get_validation_call("actual-proof".into())
            .await
            .unwrap()
            .expect("production execution registers its proof");
        assert_eq!(call.status, ValidationCallStatus::Succeeded);
        assert_eq!(
            call.evidence.validation_result.as_ref().unwrap()["status"],
            "succeeded"
        );
        let receipt = fixture
            .store
            .submit_agent_receipt(
                fixture.attempt.attempt_id,
                ReceiptDraft {
                    status: AgentStatusClaim::Completed,
                    summary: "command ran".into(),
                    criterion_results: vec![CriterionResult {
                        criterion_id: "execution".into(),
                        status: CriterionStatus::Passed,
                        evidence: None,
                        evidence_ref: Some(CriterionEvidenceRef {
                            call_id: call.call_id.clone(),
                            workspace_id: fixture.assignment.workspace_id.clone(),
                            evidence_epoch: call.evidence.end_epoch.unwrap(),
                            kind: CriterionEvidenceKind::ValidationExecution,
                        }),
                    }],
                    declared_changes: Vec::new(),
                    validation_call_ids: vec![call.call_id],
                    blockers: Vec::new(),
                    risks: Vec::new(),
                    next_action: None,
                    architecture_contract: None,
                },
            )
            .await
            .expect("real successful execution permits receipt sealing");
        assert_eq!(receipt.status, AgentStatusClaim::Completed);
        assert_eq!(receipt.criterion_results[0].status, CriterionStatus::Passed);
    }
}

#[tokio::test]
async fn failed_commands_and_cancelled_owners_do_not_supply_successful_proof() {
    for unified in [false, true] {
        let fixture = Fixture::new(vec!["exit 7".into(), "cancel me".into()]).await;
        fixture.command(unified, "failed-proof", "exit 7").await;
        assert_eq!(
            fixture
                .store
                .get_validation_call("failed-proof".into())
                .await
                .unwrap()
                .unwrap()
                .status,
            ValidationCallStatus::Failed
        );
        let guard = TaskValidation::start(
            &fixture.session,
            &fixture.turn,
            "cancelled-proof",
            "cancel me",
            &["test".into()],
            Some(fixture.repo.path()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        drop(guard);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fixture
                    .store
                    .get_validation_call("cancelled-proof".into())
                    .await
                    .unwrap()
                    .unwrap()
                    .status
                    == ValidationCallStatus::Cancelled
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropped owner settles its lease");
        assert!(
            TaskValidation::start(
                &fixture.session,
                &fixture.turn,
                "unrelated",
                "not required",
                &["test".into()],
                Some(fixture.repo.path()),
                None
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            fixture
                .store
                .get_validation_call("unrelated".into())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            TaskValidation::start(
                &fixture.session,
                &fixture.turn,
                "foreign",
                "cancel me",
                &["test".into()],
                None,
                None
            )
            .await
            .is_err()
        );
        assert!(
            fixture
                .store
                .get_validation_call("foreign".into())
                .await
                .unwrap()
                .is_none()
        );
    }
}
