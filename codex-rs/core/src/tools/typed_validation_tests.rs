use super::*;
use crate::session::step_context::StepContext;
use crate::tools::context::ExecCommandToolOutput;
use codex_agent_task_store::*;
use serde_json::json;

/// Mock only the native process result, not the registry-to-store producer or
/// its proof parser. No validation_calls are inserted by the fixture.
struct NativeValidationResult {
    case: &'static str,
    origin: String,
}

impl ToolExecutor<ToolInvocation> for NativeValidationResult {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("exec_command")
    }
    fn spec(&self) -> ToolSpec {
        crate::tools::handlers::ExecCommandHandler::default().spec()
    }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let mut argv = vec![
                "cargo".to_string(),
                "test".to_string(),
                "focused_case".to_string(),
            ];
            if self.case == "unknown_obligation" {
                argv[2] = "different_case".into();
            }
            let mut validation = crate::validation::resolve_command_validation(
                &codex_command_analysis::command_shape::CommandInvocation::Argv {
                    program: argv[0].clone(),
                    args: argv[1..].to_vec(),
                },
                Some(invocation.step_context.turn.cwd().as_path()),
                Some(codex_protocol::validation::ValidationCommandContext {
                    covered_paths: vec!["src".into()],
                }),
            )
            .await
            .unwrap();
            validation.receipt_runner = Some("rust_test_runner".into());
            let primary = invocation.step_context.environments.primary().unwrap();
            validation.execution_context = Some(crate::validation::ValidationExecutionContext {
                environment_id: if self.case == "wrong_environment" {
                    "other".into()
                } else {
                    primary.environment_id.clone()
                },
                cwd: Some(if self.case == "nested_workdir" {
                    primary.cwd().to_abs_path().unwrap().as_path().join("src")
                } else {
                    primary.cwd().to_abs_path().unwrap().to_path_buf()
                }),
                command: argv,
                environment_fingerprint: "fixture".into(),
            });
            let mut receipt = json!({
                "kind": "codex_test_execution_v1", "runner": "rust_test_runner",
                "runner_input_fingerprint": "a".repeat(64), "exit_code": 0,
                "selected_targets": ["core_lib"], "executed_tests": 1,
                "completed_tests": {"codex-core": ["focused_case"]},
            });
            if self.case == "zero_tests" {
                receipt["executed_tests"] = json!(0);
                receipt["completed_tests"] = json!({});
            }
            if self.case == "busy" {
                receipt = json!({
                    "kind": "codex_rust_admission_v1", "status": "busy", "exit_code": 75,
                    "validation_status": "pending", "executed": false, "queued": false,
                    "automatic_retry": false, "automatic_resume": false, "resource": "fixture",
                });
            }
            if self.case == "stale" {
                invocation.tracker.lock().await.record_unknown_mutation();
            }
            if self.case == "git_unchanged_end" {
                // Command completion records an uncertain command that left
                // the Git-visible tree unchanged before this result returns.
                invocation
                    .tracker
                    .lock()
                    .await
                    .record_git_unchanged_command_end_at(
                        &["cargo".to_string(), "test".to_string()],
                        "",
                        None,
                        crate::turn_diff_tracker::CommandMutation::KnownMutation {
                            paths: Some(std::collections::BTreeSet::new()),
                        },
                    );
            }
            if self.case == "cancelled" {
                invocation.cancellation_token.cancel();
            }
            let running = self.case == "running";
            Ok(Box::new(ExecCommandToolOutput {
                process_output: None,
                error: None,
                validation: Some(validation),
                event_call_id: self.origin.clone(),
                chunk_id: String::new(),
                wall_time: Duration::ZERO,
                raw_output: if self.case == "malformed" {
                    b"test result: ok. 0 passed".to_vec()
                } else {
                    receipt.to_string().into_bytes()
                },
                output_ranges: None,
                truncation_policy: invocation
                    .step_context
                    .turn
                    .model_info
                    .truncation_policy
                    .into(),
                max_output_tokens: None,
                process_id: running.then_some(7),
                session_capabilities: None,
                exit_code: (!running).then_some(if self.case == "busy" {
                    75
                } else if self.case == "failed" {
                    1
                } else {
                    0
                }),
                process_exited: !running,
                search_no_match: false,
                original_token_count: None,
                hook_command: None,
                raw_output_artifact: None,
                repair_notice: None,
                pending_deferred_completions: Vec::new(),
            }) as Box<dyn ToolOutput>)
        })
    }
}
impl CoreToolRuntime for NativeValidationResult {}

#[tokio::test]
async fn native_results_reach_owned_task_evidence_without_preseeded_calls() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let coordinator = session.services.agent_control.task_coordinator();
    let home = tempfile::tempdir().unwrap();
    let state = codex_state::StateRuntime::init(home.path().into(), "test".into())
        .await
        .unwrap();
    coordinator.initialize(state, "root".into()).await.unwrap();
    let (assignment, attempt) = coordinator
        .create_assignment(
            turn.cwd().as_path(),
            AssignmentDraft {
                root_session_id: "root".into(),
                admission_origin: AssignmentAdmissionOrigin::Typed,
                role: AgentRole::Worker,
                capability_profile: CapabilityProfile::ScopedSourceWrite,
                objective: "exercise command handoff".into(),
                acceptance_criteria: vec![AcceptanceCriterion {
                    evidence_policy: None,
                    id: "check".into(),
                    text: "focused check completes".into(),
                }],
                read_scope: Vec::new(),
                write_scope: Vec::new(),
                stop_condition: "report evidence".into(),
                dependencies: Vec::new(),
                risk_hints: Vec::new(),
                required_evidence: vec!["cargo test focused_case".into()],
                prohibited_changes: Vec::new(),
                contract_claims: Vec::new(),
                workspace_strategy: WorkspaceStrategy::Shared,
                relation: None,
                architecture_contract_ref: None,
            },
        )
        .await
        .unwrap();
    let binding = AgentTaskBinding {
        assignment_id: assignment.assignment_id,
        attempt_id: attempt.attempt_id,
        root_session_id: "root".into(),
        agent_path: "/root/worker".into(),
        task_name: "worker".into(),
        thread_id: Some(session.thread_id.to_string()),
        bound_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    turn.agent_task_binding.set(Some(binding)).unwrap();
    let store = coordinator.store().unwrap();
    let session = Arc::new(session);
    let step = StepContext::for_test(Arc::new(turn));
    assert!(
        store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap()
            .validation_calls
            .is_empty()
    );
    let tracker = Arc::new(tokio::sync::Mutex::new(
        crate::turn_diff_tracker::TurnDiffTracker::new(),
    ));
    for (case, expected) in [
        ("success", Some(ValidationCallStatus::Succeeded)),
        ("git_unchanged_end", Some(ValidationCallStatus::Succeeded)),
        ("nested_workdir", Some(ValidationCallStatus::Succeeded)),
        ("malformed", Some(ValidationCallStatus::NotExecuted)),
        ("zero_tests", Some(ValidationCallStatus::NotExecuted)),
        ("failed", Some(ValidationCallStatus::Failed)),
        ("busy", None),
        ("unknown_obligation", None),
        ("cancelled", Some(ValidationCallStatus::Cancelled)),
        ("wrong_environment", None),
        ("stale", Some(ValidationCallStatus::NotExecuted)),
        ("running", Some(ValidationCallStatus::Running)),
        ("unregistered_poll", None),
    ] {
        let invocation = ToolInvocation {
            session: Arc::clone(&session),
            step_context: Arc::clone(&step),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::clone(&tracker),
            call_id: case.into(),
            tool_name: ToolName::plain(if case == "unregistered_poll" {
                "write_stdin"
            } else {
                "exec_command"
            }),
            source: crate::tools::context::ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: "{}".into(),
            },
        };
        let output = handle_any_tool(
            &NativeValidationResult {
                case,
                origin: case.into(),
            },
            invocation,
        )
        .await
        .unwrap();
        let call = store
            .get_validation_call(format!("command:{}:{case}", session.thread_id))
            .await
            .unwrap();
        assert_eq!(call.as_ref().map(|call| call.status), expected, "{case}");
        if expected == Some(ValidationCallStatus::Succeeded) {
            assert_eq!(
                call.unwrap().verified_evidence_kind(),
                Some(CriterionEvidenceKind::ValidationExecution)
            );
            assert!(
                serde_json::to_string(&output.response())
                    .unwrap()
                    .contains("Typed validation evidence")
            );
        }
    }
    let poll = ToolInvocation {
        session: Arc::clone(&session),
        step_context: Arc::clone(&step),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker,
        call_id: "poll".into(),
        tool_name: ToolName::plain("write_stdin"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".into(),
        },
    };
    for _ in 0..2 {
        handle_any_tool(
            &NativeValidationResult {
                case: "success",
                origin: "running".into(),
            },
            poll.clone(),
        )
        .await
        .unwrap();
    }
    let task = store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .unwrap();
    assert_eq!(
        task.validation_calls.len(),
        9,
        "polls settle the original record, never add a second execution"
    );
    assert_eq!(
        task.validation_calls
            .iter()
            .find(|call| call.call_id.ends_with(":running"))
            .unwrap()
            .status,
        ValidationCallStatus::Succeeded
    );
}
