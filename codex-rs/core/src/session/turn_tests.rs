use super::*;
use crate::tools::context::ToolPayload;
use codex_protocol::protocol::SubAgentSource;

#[tokio::test]
async fn audit115_accepted_batch_preserves_correction_through_plan_handler() {
    use crate::tools::registry::ToolExecutor;
    for textless in [false, true] {
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let invoke = |arguments: serde_json::Value| crate::tools::context::ToolInvocation {
            session: Arc::clone(&session),
            step_context: crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "audit115-plan".into(),
            tool_name: codex_tools::ToolName::plain("update_plan"),
            source: crate::tools::context::ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: arguments.to_string(),
            },
        };
        crate::tools::handlers::PlanHandler
            .handle(invoke(serde_json::json!({"plan":[
                {"step":"Deploy","status":"pending"},{"step":"Review","status":"pending"}
            ]})))
            .await
            .unwrap();
        let correction = "Do not deploy; review only.";
        let input = |content| TurnInput::UserInput {
            content,
            client_id: None,
        };
        run_hooks_and_record_inputs_detailed(
            &session,
            &turn,
            vec![
                input(vec![UserInput::Text {
                    text: correction.into(),
                    text_elements: Vec::new(),
                }]),
                input(if textless {
                    vec![UserInput::Image {
                        image_url: "data:image/png;base64,AA==".into(),
                        detail: None,
                    }]
                } else {
                    vec![UserInput::Text {
                        text: "What have you found so far?".into(),
                        text_elements: Vec::new(),
                    }]
                }),
            ],
        )
        .await
        .unwrap();
        assert!(serde_json::to_string(session.clone_history().await.raw_items()).unwrap().contains(correction));
        let snapshot = session
            .services
            .plan_store
            .execution_snapshot()
            .await
            .unwrap();
        crate::tools::handlers::PlanHandler
            .handle(invoke(serde_json::json!({
                "expected_revision":snapshot.revision,
                "plan":[{"step":"Review","status":"pending"}],
            })))
            .await
            .unwrap();
        let remaining = session
            .services
            .plan_store
            .execution_snapshot()
            .await
            .unwrap();
        assert_eq!(
            remaining.obligations.unresolved.len(),
            1,
            "the status question/image cannot cancel Review"
        );
    }
}

#[test]
fn ordinary_final_preserves_plan_debt_without_reopening_the_model() -> Result<()> {
    run_turn_multi_thread_test_with_stack("audit115-final", || async {
        crate::test_harness::require_network!();
        for pending in [false, true] {
            let server = responses::start_mock_server().await;
            let sequence = vec![responses::sse(vec![
                responses::ev_assistant_message("premature", "done"),
                responses::ev_completed("first"),
            ])];
            let requests = responses::mount_sse_sequence(&server, sequence).await;
            // Mailbox recording belongs to an admitted active task. Calling
            // run_turn on the bare session fixture aborts before model dispatch.
            let test = test_codex()
                .with_config(|config| {
                    config.features.disable(Feature::CodeModeHost).unwrap();
                })
                .build(&server)
                .await?;
            let session = &test.codex.as_ref().codex.session;
            if pending {
                session
                    .services
                    .plan_store
                    .update(codex_protocol::plan_tool::UpdatePlanArgs {
                        explanation: None,
                        plan: vec![codex_protocol::plan_tool::PlanItemArg {
                            step: "Required wiring".into(),
                            status: codex_protocol::plan_tool::StepStatus::Pending,
                        }],
                    })
                    .await;
            }
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                test.submit_turn_and_capture_completion(
                    "Finish the required wiring; do not run tests.",
                ),
            )
            .await??;
            assert!(result.error.is_none(), "{result:?}");
            assert_eq!(requests.requests().len(), 1);
            assert_eq!(
                result.last_agent_message.as_deref(),
                Some("done")
            );
            if pending {
                let snapshot = session.services.plan_store.execution_snapshot().await.unwrap();
                assert_eq!(snapshot.obligations.unresolved.len(), 1,
                    "delivery must not silently discharge unfinished work");
            }
            assert!(
                result
                    .timing
                    .as_ref()
                    .and_then(|timing| timing.final_answer.as_ref())
                    .and_then(|answer| answer.message.as_ref())
                    .is_some()
            );
        }
        Ok(())
    })
}

#[tokio::test]
async fn mailbox_reports_preserve_passive_cycles_but_not_instruction_or_evidence_changes() {
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_tools::ToolName;
    use codex_tools::ToolOutputOutcomeContext;
    for arm in [
        "none",
        "chatter",
        "owner",
        "mutation",
        "exposure",
        "changed-result",
    ] {
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let mut control = TurnExecutionControl::new();
        let mut advice = Vec::new();
        for generation in 0..4 {
            if generation > 0 && arm != "none" {
                let mut mail = codex_protocol::protocol::InterAgentCommunication::new(
                    codex_protocol::AgentPath::try_from(if arm == "owner" {
                        "/root"
                    } else {
                        "/root/worker"
                    })
                    .unwrap(),
                    codex_protocol::AgentPath::try_from(if arm == "owner" {
                        "/root/worker"
                    } else {
                        "/root"
                    })
                    .unwrap(),
                    Vec::new(),
                    "unchanged report".to_owned(),
                    true,
                );
                mail.id = Some(codex_protocol::ResponseItemId::with_suffix(
                    "report", generation,
                ));
                assert!(
                    session
                        .input_queue
                        .enqueue_mailbox_communication(mail.clone())
                        .await
                        .unwrap()
                );
                assert!(
                    !session
                        .input_queue
                        .enqueue_mailbox_communication(mail)
                        .await
                        .unwrap()
                );
                let input = session
                    .input_queue
                    .take_mailbox_for_initial_input(&session.active_turn, &[])
                    .await;
                assert_eq!(input.len(), 1, "distinct-ID mail delivered exactly once");
                let recorded = run_hooks_and_record_inputs_detailed(&session, &turn, input)
                    .await
                    .unwrap();
                assert!(recorded.accepted_context_input);
                assert_eq!(recorded.instructions_changed, arm == "owner");
                assert_eq!(recorded.renews_generation_budget, arm == "owner");
                control.accepted_context_input(recorded.instructions_changed);
            }
            let mutation = if arm == "mutation" { generation } else { 0 };
            let exposure = if arm == "exposure" { generation } else { 0 };
            let baselines = control.baselines_with_tool_exposure_revision(mutation, exposure);
            let collector = control.collector(&baselines);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("read_tool_output"), &ToolPayload::Function {
                    arguments: r#"{"artifact_id":"fixed","selectors":[{"kind":"lines","start":1,"end":1}]}"#.into(),
                }, "failure",
            );
            let fingerprint = if arm == "changed-result" && generation > 0 {
                "new external evidence"
            } else {
                "same failure"
            };
            collector.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(codex_tools::ToolOutputOutcome::Failure),
                Some(
                    serde_json::json!({"outcome":"failure", "failure":{"fingerprint":fingerprint}}),
                ),
                &ResponseInputItem::FunctionCallOutput {
                    call_id: "failure".into(),
                    output: FunctionCallOutputPayload::from_text(fingerprint.into()),
                },
                false,
            );
            let decision = control.evaluate_convergence(
                &baselines,
                &collector,
                &SamplingRequestSettledState {
                    mutation_revision: mutation,
                    attributed_mutation_revision: mutation,
                    tool_exposure_revision: exposure,
                },
            );
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            advice.push(decision.directive.is_some());
        }
        match arm {
            "none" | "chatter" => assert_eq!(advice, vec![false, true, true, true], "{arm}"),
            "owner" | "mutation" | "exposure" => assert_eq!(advice, vec![false; 4], "{arm}"),
            _ => assert!(!advice[1], "a changed dependency result is fresh evidence"),
        }
    }
}

#[tokio::test]
async fn terminal_hook_effects_invalidate_the_returned_assessment() {
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;
    use codex_config::ConfigLayerStack;
    use codex_config::ConfigRequirements;
    use codex_config::ConfigRequirementsToml;
    for subagent in [false, true] {
        for ending in ["", "raise SystemExit(1)", "import time; time.sleep(30)"] {
            let (session, mut turn, _events) =
                crate::session::tests::make_session_and_context_with_rx().await;
            if subagent {
                Arc::get_mut(&mut turn).unwrap().session_source =
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                        parent_thread_id: codex_protocol::ThreadId::new(),
                        depth: 1,
                        agent_path: None,
                        agent_nickname: None,
                        agent_role: None,
                    });
            }
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("tested.py");
            fs::write(&source, "tested source").unwrap();
            let script = temp.path().join("hook.py");
            fs::write(&script, format!("from pathlib import Path\np = Path({})\np.write_text(p.read_text() + ' changed')\n{ending}\n",
                serde_json::to_string(&source.to_string_lossy()).unwrap())).unwrap();
            let event = if subagent { "SubagentStop" } else { "Stop" };
            let mut hooks = serde_json::Map::new();
            hooks.insert(
                event.into(),
                serde_json::json!([{"hooks":[{"type":"command",
                "command":format!("python3 \"{}\"", script.display()),
                "commandWindows":format!("python \"{}\"", script.display()), "timeout":1}]}]),
            );
            let layer = serde_json::from_value(serde_json::json!({"hooks":hooks})).unwrap();
            let stack = ConfigLayerStack::new(
                vec![ConfigLayerEntry::new(
                    ConfigLayerSource::User {
                        file: turn.config.codex_home.join("config.toml"),
                        profile: None,
                    },
                    layer,
                )],
                ConfigRequirements::default(),
                ConfigRequirementsToml::default(),
            )
            .unwrap();
            session
                .services
                .hooks
                .store(Arc::new(codex_hooks::Hooks::new(
                    codex_hooks::HooksConfig {
                        feature_enabled: true,
                        bypass_hook_trust: true,
                        config_layer_stack: Some(stack),
                        ..Default::default()
                    },
                )));
            let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
            let report = run_completion_stop_hook(
                &session,
                &turn,
                false,
                Some("done".into()),
                || std::future::ready(Default::default()),
                &tracker,
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(
                fs::read_to_string(source).unwrap(),
                "tested source changed",
                "hook runs once"
            );
            assert!(
                !report.assessment.verification_gaps.is_empty(),
                "{event}: {ending}"
            );
            assert!(tracker.lock().await.current_mutation_revision() > 0);
        }
    }
}

#[tokio::test]
async fn no_terminal_hook_preserves_assessment_and_mutation_revision() {
    let (session, turn, _events) = crate::session::tests::make_session_and_context_with_rx().await;
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let report = run_completion_stop_hook(
        &session,
        &turn,
        false,
        None,
        || std::future::ready(Default::default()),
        &tracker,
        &CancellationToken::new(),
    )
    .await;
    assert!(report.assessment.verification_gaps.is_empty());
    assert_eq!(tracker.lock().await.current_mutation_revision(), 0);
}

#[test]
fn uncertainty_completion_warnings_use_owner_assessment_not_final_wording() {
    let mut assessment = codex_protocol::protocol::TurnCompletionAssessment::default();
    assert!(completion_verification_warning(&assessment).is_none());
    assessment
        .advisories
        .push("Checklist accounting only".into());
    assert!(completion_verification_warning(&assessment).is_none());
    assessment
        .verification_gaps
        .push("Required compatibility remains untested.".into());
    let warning = completion_verification_warning(&assessment).unwrap();
    assert!(warning.contains("Required compatibility remains untested."));
    assert!(warning.contains("not a verified completion"));
    assessment.verification_gaps.clear();
    assessment.failed_checks.push("Reader V1 failed".into());
    assert!(
        completion_verification_warning(&assessment)
            .unwrap()
            .contains("Reader V1 failed")
    );
    assessment.failed_checks.clear();
    assert!(completion_verification_warning(&assessment).is_none());
}
use crate::session::turn_execution::AuthoritativeWaitOwnerResult;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use crate::test_harness::hooks::trust_discovered_hooks;
use crate::test_harness::responses;
use crate::test_harness::test_codex::test_codex;
use crate::tools::exposure::AgentSurfaceStage;
use crate::tools::exposure::EnvironmentSurfaceMode;
use crate::tools::exposure::GoalSurfaceState;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use anyhow::Context as _;
use anyhow::Result;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnInputContributor;
use codex_extension_api::TurnItemContributor;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use indexmap::IndexMap;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Tool;
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn missing_stabilized_context_update_returns_a_fatal_error() {
    let mut prepared_context_update = None;

    let result = take_stabilized_context_update(&mut prepared_context_update);

    let Err(CodexErr::Fatal(message)) = result else {
        panic!("missing stabilized context must return a fatal error");
    };
    assert!(message.contains("did not carry its context candidate"));
}
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Debug)]
struct ContendedCatalogModelsManager {
    inner: codex_models_manager::manager::SharedModelsManager,
    catalog_gate: Arc<tokio::sync::RwLock<()>>,
    read_attempted: Arc<std::sync::atomic::AtomicBool>,
}

impl codex_models_manager::manager::ModelsManager for ContendedCatalogModelsManager {
    fn model_catalog_activity(&self) -> Arc<codex_models_manager::manager::ModelCatalogActivity> {
        self.inner.model_catalog_activity()
    }

    fn refresh_models_for_background(
        &self,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<'_, codex_protocol::error::Result<()>>
    {
        self.inner
            .refresh_models_for_background(http_client_factory)
    }

    fn list_models_shared(
        &self,
        refresh_strategy: codex_models_manager::manager::RefreshStrategy,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<
        '_,
        codex_protocol::error::Result<Arc<Vec<codex_protocol::openai_models::ModelPreset>>>,
    > {
        let inner = Arc::clone(&self.inner);
        let catalog_gate = Arc::clone(&self.catalog_gate);
        let read_attempted = Arc::clone(&self.read_attempted);
        Box::pin(async move {
            read_attempted.store(true, Ordering::Release);
            let catalog_read = catalog_gate.read().await;
            drop(catalog_read);
            inner
                .list_models_shared(refresh_strategy, http_client_factory)
                .await
        })
    }

    fn raw_model_catalog(
        &self,
        refresh_strategy: codex_models_manager::manager::RefreshStrategy,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<
        '_,
        codex_protocol::error::Result<ModelsResponse>,
    > {
        self.inner
            .raw_model_catalog(refresh_strategy, http_client_factory)
    }

    fn get_remote_models(
        &self,
    ) -> codex_models_manager::manager::ModelsManagerFuture<
        '_,
        Vec<codex_protocol::openai_models::ModelInfo>,
    > {
        self.inner.get_remote_models()
    }

    fn try_get_remote_models(
        &self,
    ) -> std::result::Result<Vec<codex_protocol::openai_models::ModelInfo>, tokio::sync::TryLockError>
    {
        self.inner.try_get_remote_models()
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.inner.auth_manager()
    }

    fn list_collaboration_modes(&self) -> Vec<codex_protocol::config_types::CollaborationModeMask> {
        self.inner.list_collaboration_modes()
    }

    fn try_list_models_shared(
        &self,
    ) -> std::result::Result<
        Arc<Vec<codex_protocol::openai_models::ModelPreset>>,
        tokio::sync::TryLockError,
    > {
        self.read_attempted.store(true, Ordering::Release);
        let _catalog_read = self.catalog_gate.try_read()?;
        self.inner.try_list_models_shared()
    }

    fn notify_etag(
        self: Arc<Self>,
        etag: String,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<'static, ()> {
        Arc::clone(&self.inner).notify_etag(etag, http_client_factory)
    }
}

#[test]
fn turn_submission_type_distinguishes_queued_continuations() {
    assert!(matches!(
        turn_submission_type(&[]),
        TurnSubmissionType::Queued
    ));

    let input = [TurnInput::UserInput {
        content: Vec::new(),
        client_id: None,
    }];
    assert!(matches!(
        turn_submission_type(&input),
        TurnSubmissionType::Default
    ));
}

#[tokio::test]
async fn stopped_session_start_restores_input_and_requests_a_fresh_turn() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "retry after session-start stop".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];

    let result = finish_stopped_session_start(&session, &turn, input.clone()).await;

    assert!(result.defer_pending_input);
    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await,
        input
    );
    let owned_turn = Arc::new(turn);
    let worker = tokio::spawn(std::future::pending::<()>());
    let active = crate::state::ActiveTurn {
        // Input ownership is tied to a running task's turn identity, not only
        // to the terminal coordinator. Match installed-startup scaffolding.
        task: Some(crate::state::RunningTask {
            done: Arc::new(tokio::sync::Notify::new()),
            worker_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kind: crate::state::TaskKind::Regular,
            task: Arc::new(crate::tasks::RegularTask::new()),
            cancellation_token: CancellationToken::new(),
            auxiliary_cancellation_token: CancellationToken::new(),
            auxiliary_tasks: tokio::task::JoinSet::new(),
            worker_abort_handle: worker.abort_handle(),
            task_span: tracing::Span::none(),
            turn_context: Arc::clone(&owned_turn),
            _agent_execution_guard: None,
        }),
        terminal: Some(crate::state::TurnTerminalCoordinator::new(
            owned_turn.sub_id.clone(),
        )),
        ..Default::default()
    };
    let state = active.turn_state.clone();
    state.lock().await.input_in_flight = input.iter().cloned().collect();
    *session.active_turn.lock().await = Some(active);
    let owned_result = finish_stopped_session_start(&session, &owned_turn, input.clone()).await;
    assert!(owned_result.defer_pending_input);
    session
        .input_queue
        .recover_pending_input_for_turn_state(&state)
        .await;
    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await,
        input,
        "an owned startup stop must recover once, not restore a second copy"
    );
    worker.abort();
    let _ = worker.await;
}

#[tokio::test]
async fn prompt_stop_hook_runs_before_context_planning_or_publication() {
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;
    use codex_config::ConfigLayerStack;
    use codex_config::ConfigRequirements;
    use codex_config::ConfigRequirementsToml;
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    Arc::get_mut(&mut turn).unwrap().multi_agent_version =
        codex_protocol::protocol::MultiAgentVersion::V2;
    let layer = serde_json::from_value(serde_json::json!({"hooks": {
        "UserPromptSubmit": [{"hooks": [{
            "type": "command",
            "command": "python3 -c \"import sys; sys.stderr.write('blocked by test'); sys.exit(2)\"",
            "commandWindows": "python -c \"import sys; sys.stderr.write('blocked by test'); sys.exit(2)\""
        }]}]
    }}))
    .unwrap();
    let stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: turn.config.codex_home.join("config.toml"),
                profile: None,
            },
            layer,
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .unwrap();
    session
        .services
        .hooks
        .store(Arc::new(codex_hooks::Hooks::new(
            codex_hooks::HooksConfig {
                feature_enabled: true,
                bypass_hook_trust: true,
                config_layer_stack: Some(stack),
                ..Default::default()
            },
        )));
    let mut budget = LogicalGenerationBudget::default();
    tokio::time::timeout(
        Duration::from_secs(10),
        run_turn(
            Arc::clone(&session),
            Arc::clone(&turn),
            Arc::new(ExtensionData::new(turn.sub_id.clone())),
            vec![TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "Use subagents to perform the blocked prompt".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            }],
            None,
            &mut budget,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("blocking hook must finish without planning or model access")
    .unwrap();
    assert!(session.clone_history().await.raw_items().is_empty());
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );
    assert_eq!(budget.regular_generations, 0);
    let mut stopped = false;
    while let Ok(event) = events.try_recv() {
        if let EventMsg::Warning(warning) = event.msg {
            stopped |= warning.message.contains("UserPromptSubmit hook stopped");
        }
    }
    assert!(stopped, "the real hook must have blocked the prompt");
    let stopped = run_hooks_and_record_inputs_detailed(
        &session,
        &turn,
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "blocked continuation".into(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }],
    )
    .await
    .unwrap();
    assert!(stopped.should_stop);
    assert!(!stopped.accepted_context_input);
    let continuing = run_hooks_and_record_inputs_detailed(
        &session,
        &turn,
        vec![
            TurnInput::ResponseItem(crate::compact::compaction_context_message(
                "Accepted ongoing context".to_string(),
            )),
            TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "Use subagents".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        ],
    )
    .await
    .unwrap();
    assert!(!continuing.should_stop);
    assert!(continuing.accepted_context_input);
    assert!(continuing.renews_generation_budget);
    // A child's result is accepted context, but it does not renew the
    // generation budget of the turn it reports to.
    let report = run_hooks_and_record_inputs_detailed(
        &session,
        &turn,
        vec![TurnInput::InterAgentCommunication(
            codex_protocol::protocol::InterAgentCommunication::new(
                codex_protocol::AgentPath::try_from("/root/worker").unwrap(),
                codex_protocol::AgentPath::root(),
                Vec::new(),
                "child result".to_string(),
                true,
            ),
        )],
    )
    .await
    .unwrap();
    assert!(!report.should_stop);
    assert!(report.accepted_context_input);
    assert!(!report.renews_generation_budget);
}

#[tokio::test]
async fn pending_input_closed_writer_restores_entire_rejected_batch() {
    let (mut session, turn, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    crate::session::tests::attach_thread_persistence(
        Arc::get_mut(&mut session).expect("unique session"),
    )
    .await;
    session
        .live_thread()
        .expect("writer")
        .shutdown()
        .await
        .expect("close writer");
    let input = ["first rejected steer", "second rejected steer"]
        .map(|text| TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: text.into(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        })
        .to_vec();
    session
        .input_queue
        .restore_transferred_startup_input(input.clone())
        .await;
    let drained = session
        .input_queue
        .get_pending_input(&session.active_turn)
        .await;
    assert_eq!(drained, input);
    assert!(
        run_hooks_and_record_inputs_detailed(&session, &turn, drained)
            .await
            .is_err()
    );
    assert!(session.clone_history().await.raw_items().is_empty());
    assert!(
        events.try_recv().is_err(),
        "rejected inputs must not publish lifecycle events"
    );
    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await,
        input
    );
    assert!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn pending_input_cancelled_waiter_keeps_prefix_and_restores_only_rejected_suffix() {
    // A real local writer, a FIFO commit gate, and no model request. Cancellation
    // occurs while the first commit is blocked; the writer then closes between items.
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    crate::session::tests::attach_thread_persistence(
        Arc::get_mut(&mut session).expect("unique session"),
    )
    .await;
    let input = ["owned prefix", "rejected suffix one", "rejected suffix two"]
        .map(|text| {
            TurnInput::ResponseItem(crate::compact::compaction_context_message(text.into()))
        })
        .to_vec();
    session
        .input_queue
        .restore_transferred_startup_input(input.clone())
        .await;
    let drained = session
        .input_queue
        .get_pending_input(&session.active_turn)
        .await;
    let held = session
        .durable_history_commit_gate
        .acquire()
        .await
        .expect("hold first commit");
    let mut waiter = Box::pin(run_hooks_and_record_inputs_detailed(
        &session, &turn, drained,
    ));
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    tokio::time::timeout(Duration::from_secs(5), async {
        while session
            .durable_history_commits_in_flight
            .load(Ordering::Acquire)
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first owned commit starts");
    // Let the just-spawned commit poll the held semaphore before queuing the
    // test's next permit. FIFO ordering then gives us the boundary after item 1.
    tokio::task::yield_now().await;
    let mut between_items = Box::pin(session.durable_history_commit_gate.acquire());
    assert!(futures::poll!(between_items.as_mut()).is_pending());
    drop(waiter);
    assert!(session.clone_history().await.raw_items().is_empty());
    assert!(
        !session.terminal_tasks.is_empty(),
        "batch owner survives waiter cancellation"
    );
    drop(held);
    let boundary = tokio::time::timeout(Duration::from_secs(5), between_items)
        .await
        .expect("first commit releases FIFO gate")
        .expect("gate open");
    let history = session.clone_history().await.into_raw_items();
    assert_eq!(
        history.len(),
        1,
        "must close the actual writer after exactly the accepted prefix"
    );
    assert!(
        serde_json::to_string(&history[0])
            .unwrap()
            .contains("owned prefix")
    );
    session
        .live_thread()
        .expect("writer")
        .shutdown()
        .await
        .expect("close writer between items");
    drop(boundary);
    session.terminal_tasks.close();
    tokio::time::timeout(Duration::from_secs(5), session.terminal_tasks.wait())
        .await
        .expect("cancelled batch owner settles failure and restores suffix");
    let restored = session
        .input_queue
        .get_pending_input(&session.active_turn)
        .await;
    assert_eq!(
        restored,
        input[1..],
        "accepted prefix must never be submitted twice"
    );
    assert!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .is_empty()
    );
    assert_eq!(session.clone_history().await.into_raw_items(), history);
    let stored = session
        .live_thread()
        .expect("writer")
        .load_history(false)
        .await
        .expect("stored history");
    let stored_items = stored
        .items
        .into_iter()
        .filter_map(|item| match item {
            codex_protocol::protocol::RolloutItem::ResponseItem(item) => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stored_items, history,
        "cancelled waiter must preserve accepted prefix exactly once on disk"
    );
}

#[tokio::test]
async fn consecutive_turn_contexts_share_the_unchanged_picker_snapshot() {
    let (session, first_turn) = crate::session::tests::make_session_and_context().await;

    let second_turn = session
        .new_default_turn_with_sub_id("shared-picker-snapshot".to_string())
        .await;

    assert!(Arc::ptr_eq(
        &first_turn.available_models,
        &second_turn.available_models
    ));
}

#[tokio::test]
async fn pending_hook_recorder_is_cancelled_before_terminal_and_successor() {
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;
    use codex_config::ConfigLayerStack;
    use codex_config::ConfigRequirements;
    use codex_config::ConfigRequirementsToml;
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("gated_prompt.py");
    let entered = directory.path().join("entered");
    let release = directory.path().join("release");
    std::fs::write(&script, "import pathlib,sys,time\npathlib.Path(sys.argv[1]).write_text('entered')\nwhile not pathlib.Path(sys.argv[2]).exists(): time.sleep(0.01)\n").unwrap();
    let command = |python: &str| {
        format!(
            "{python} \"{}\" \"{}\" \"{}\"",
            script.display(),
            entered.display(),
            release.display()
        )
    };
    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let layer = serde_json::from_value(serde_json::json!({"hooks": {"UserPromptSubmit": [{"hooks": [{
        "type": "command", "command": command("python3"), "commandWindows": command("python"), "timeout": 120
    }]}]}})).unwrap();
    let stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: turn.config.codex_home.join("config.toml"),
                profile: None,
            },
            layer,
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .unwrap();
    session
        .services
        .hooks
        .store(Arc::new(codex_hooks::Hooks::new(
            codex_hooks::HooksConfig {
                feature_enabled: true,
                bypass_hook_trust: true,
                config_layer_stack: Some(stack),
                ..Default::default()
            },
        )));
    session
        .spawn_task(
            turn.clone(),
            Vec::new(),
            SignalCompletingTask {
                finish: CancellationToken::new(),
            },
        )
        .await;
    session
        .steer_input(
            vec![UserInput::Text {
                text: "older-A".into(),
                text_elements: Vec::new(),
            }],
            Default::default(),
            Some(&turn.sub_id),
            None,
            None,
        )
        .await
        .unwrap();
    let mut recorder = Box::pin(drain_and_record_pending_inputs(&session, &turn, false, &[]));
    assert!(futures::poll!(recorder.as_mut()).is_pending());
    tokio::time::timeout(Duration::from_secs(10), async {
        while !entered.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("real prompt hook entered before any commit");
    assert_eq!(
        session
            .durable_history_commits_in_flight
            .load(Ordering::Acquire),
        0
    );
    drop(recorder);
    tokio::time::timeout(
        Duration::from_secs(5),
        session.abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted),
    )
    .await
    .expect("interruption must not wait for the hook timeout");
    let mut terminal = false;
    while let Ok(event) = events.try_recv() {
        terminal |= matches!(event.msg, EventMsg::TurnAborted(_));
    }
    assert!(terminal);
    let next = session
        .new_default_turn_with_sub_id("successor".into())
        .await;
    session
        .record_user_prompt_and_emit_turn_item(
            &next,
            &[UserInput::Text {
                text: "newer-B".into(),
                text_elements: Vec::new(),
            }],
            None,
        )
        .await
        .unwrap();
    std::fs::write(release, "release").unwrap();
    session.terminal_tasks.close();
    tokio::time::timeout(Duration::from_secs(5), session.terminal_tasks.wait())
        .await
        .unwrap();
    let history = serde_json::to_string(session.clone_history().await.raw_items()).unwrap();
    assert!(
        !history.contains("older-A"),
        "discarded old hook input must never append after terminal"
    );
    assert_eq!(history.matches("newer-B").count(), 1);
}

#[tokio::test]
async fn aborted_undelivered_context_is_present_once_on_the_next_explicit_input() {
    for kind in [
        AdditionalContextKind::Untrusted,
        AdditionalContextKind::Application,
    ] {
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        session
            .spawn_task(
                turn.clone(),
                Vec::new(),
                SignalCompletingTask {
                    finish: CancellationToken::new(),
                },
            )
            .await;
        let context = indexmap::IndexMap::from([(
            "browser_info".to_string(),
            AdditionalContextEntry {
                kind,
                value: "This tab is the production dashboard.".to_string(),
            },
        )]);
        session
            .steer_input(
                vec![UserInput::Text {
                    text: "older-A".into(),
                    text_elements: Vec::new(),
                }],
                context.clone(),
                Some(&turn.sub_id),
                None,
                None,
            )
            .await
            .unwrap();
        session
            .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
            .await;
        let next = session
            .new_default_turn_with_sub_id("context-successor".into())
            .await;
        session
            .record_additional_context_snapshot(&next, context.clone())
            .await
            .unwrap();
        session
            .record_user_prompt_and_emit_turn_item(
                &next,
                &[UserInput::Text {
                    text: "newer-B".into(),
                    text_elements: Vec::new(),
                }],
                None,
            )
            .await
            .unwrap();
        session
            .record_additional_context_snapshot(&next, context)
            .await
            .unwrap();
        let history = serde_json::to_string(session.clone_history().await.raw_items()).unwrap();
        assert_eq!(
            history
                .matches("This tab is the production dashboard.")
                .count(),
            1
        );
        assert!(history.find("production dashboard.").unwrap() < history.find("newer-B").unwrap());
        assert!(!history.contains("older-A"));
    }
}

#[tokio::test]
async fn sampling_uses_latest_tier_instead_of_the_turn_start_snapshot() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config)
        .features
        .enable(Feature::FastMode)
        .unwrap();
    turn.model_info.service_tiers = vec![codex_protocol::openai_models::ModelServiceTier {
        id: "priority".into(),
        name: "Priority".into(),
        description: "Priority processing".into(),
    }];
    Arc::make_mut(&mut turn.config).service_tier = Some("default".into());
    {
        let mut state = session.state.lock().await;
        for tier in ["priority", "default", "priority"] {
            state.session_configuration.service_tier = Some(tier.into());
        }
    }
    assert_eq!(
        service_tier_for_sampling(&session, &turn).await.as_deref(),
        Some("priority")
    );
    session
        .state
        .lock()
        .await
        .session_configuration
        .service_tier = None;
    assert_eq!(service_tier_for_sampling(&session, &turn).await, None);
}

#[tokio::test]
// This test intentionally keeps the writer guard while awaiting the blocked reader.
#[allow(clippy::await_holding_invalid_type, clippy::await_holding_lock)]
async fn turn_context_waits_for_a_contended_picker_snapshot() {
    let (mut session, first_turn) = crate::session::tests::make_session_and_context().await;
    let expected_models = Arc::clone(&first_turn.available_models);
    let catalog_gate = Arc::new(tokio::sync::RwLock::new(()));
    let read_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    session.services.models_manager = Arc::new(ContendedCatalogModelsManager {
        inner: Arc::clone(&session.services.models_manager),
        catalog_gate: Arc::clone(&catalog_gate),
        read_attempted: Arc::clone(&read_attempted),
    });
    let catalog_write = catalog_gate.write().await;
    let session = Arc::new(session);
    let mut next_turn = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            session
                .new_default_turn_with_sub_id("contended-picker-snapshot".to_string())
                .await
        }
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while !read_attempted.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn construction should attempt to read the picker catalog");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut next_turn)
            .await
            .is_err(),
        "turn construction must wait for a contended catalog instead of publishing an empty one"
    );

    drop(catalog_write);
    let next_turn = tokio::time::timeout(Duration::from_secs(1), next_turn)
        .await
        .expect("turn construction should resume after the catalog writer exits")
        .expect("turn construction task should succeed");
    assert_eq!(
        next_turn.available_models.as_ref(),
        expected_models.as_ref()
    );
}

#[tokio::test]
async fn projected_prompt_shares_prepared_projection_storage_across_appends() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    );
    let tool_search_output = ResponseItem::ToolSearchOutput {
        id: None,
        call_id: None,
        status: "completed".to_string(),
        execution: "server".to_string(),
        tools: vec![serde_json::json!({
            "type": "function",
            "name": "search_repository",
            "description": "large acknowledged schema",
            "parameters": {"type": "object"}
        })],
        omitted_result_count: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let reasoning = ResponseItem::Reasoning {
        id: None,
        summary: Vec::new(),
        content: None,
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let mut history = ContextManager::new();
    history.record_items([&tool_search_output], TruncationPolicy::Tokens(10_000));
    for append in [None, Some(&reasoning)] {
        if let Some(item) = append {
            history.record_items([item], TruncationPolicy::Tokens(10_000));
        }
        let prepared = history
            .clone()
            .prepare_for_sampling_prompt(&[], StableContextTarget::Sampling);
        let prompt = build_projected_prompt(
            &session,
            &prepared,
            &router,
            step_context.as_ref(),
            session.get_base_instructions().await,
        );

        // Retry bookkeeping holds `shared_items`; the request must send that
        // copy rather than flattening the whole history a second time.
        assert!(Arc::ptr_eq(&prompt.input, &prepared.shared_items()));
        assert!(Arc::ptr_eq(
            &prompt.stable_context_fallback_input,
            &prepared.shared_fallback_items()
        ));
        assert!(Arc::ptr_eq(
            &prompt.tool_history_fallback_input,
            &prepared.shared_unreplaced_items()
        ));
        assert!(Arc::ptr_eq(
            &prompt.stable_context_tool_history_fallback_input,
            &prepared.shared_unreplaced_fallback_items()
        ));
        assert_eq!(
            prompt.input[0], tool_search_output,
            "acknowledged search schemas keep their cached-prefix bytes"
        );
        assert_eq!(prompt.input.len(), 1 + usize::from(append.is_some()));
    }
}
#[test]
fn arc_identity_short_circuits_equivalence_check() {
    let comparison_count = AtomicUsize::new(0);
    let shared = Arc::new([1_u8, 2, 3]);
    let shared_alias = Arc::clone(&shared);

    assert!(arc_identity_or_equivalent(
        &shared,
        &shared_alias,
        |left, right| {
            comparison_count.fetch_add(1, Ordering::Relaxed);
            left == right
        }
    ));
    assert_eq!(comparison_count.load(Ordering::Relaxed), 0);

    let equal_but_distinct = Arc::new([1_u8, 2, 3]);
    assert!(arc_identity_or_equivalent(
        &shared,
        &equal_but_distinct,
        |left, right| {
            comparison_count.fetch_add(1, Ordering::Relaxed);
            left == right
        }
    ));
    assert_eq!(comparison_count.load(Ordering::Relaxed), 1);
    let unequal = Arc::new([1_u8, 2, 4]);
    assert!(!arc_identity_or_equivalent(
        &shared,
        &unequal,
        |left, right| {
            comparison_count.fetch_add(1, Ordering::Relaxed);
            left == right
        }
    ));
    assert_eq!(comparison_count.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn request_scaffold_reuses_stable_preparation_and_invalidates_only_owner_changes() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let equivalent_turn_context = session
        .new_default_turn_with_sub_id("equivalent-scaffold-owner".to_string())
        .await;
    assert!(
        Arc::ptr_eq(&turn_context.config, &equivalent_turn_context.config),
        "value-equivalent consecutive turns should share the interned config allocation"
    );
    assert_eq!(
        turn_context.config.as_ref(),
        equivalent_turn_context.config.as_ref(),
        "the cross-turn test requires value-equivalent config owners"
    );
    let equivalent_step_context = session
        .capture_step_context(equivalent_turn_context)
        .await
        .unwrap();
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    ));
    let base_instructions = session.get_base_instructions().await;
    let mut history = ContextManager::new();
    let first_user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "first dynamic request".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(
        [&first_user_message],
        turn_context.model_info.truncation_policy.into(),
    );
    let first_prepared = history.clone().prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    let second_user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "second dynamic request".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(
        [&second_user_message],
        turn_context.model_info.truncation_policy.into(),
    );
    let second_prepared = history.prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    assert_ne!(first_prepared.fingerprint(), second_prepared.fingerprint());
    assert!(stable_context_owner_matches(
        first_prepared.stable_context_manifest(),
        second_prepared.stable_context_manifest(),
    ));

    let mut cache = session
        .request_scaffold_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = cache.resolve(
        &first_prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    let first_scaffold = Arc::clone(&first.scaffold);
    let first_prompt =
        build_projected_prompt_from_scaffold(&first_prepared, step_context.as_ref(), &first);
    drop(cache);
    let mut cache = session
        .request_scaffold_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let second = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(second.locally_reused);
    assert!(Arc::ptr_eq(&first_scaffold, &second.scaffold));
    assert_eq!(cache.build_count(), 1);
    let second_prompt =
        build_projected_prompt_from_scaffold(&second_prepared, step_context.as_ref(), &second);
    assert_ne!(first_prompt.input, second_prompt.input);
    assert_eq!(
        first_prompt.digests.instructions,
        second_prompt.digests.instructions
    );
    assert_eq!(first_prompt.digests.tools, second_prompt.digests.tools);
    assert_ne!(first_prompt.digests.history, second_prompt.digests.history);
    assert!(Arc::ptr_eq(&first_prompt.tools, &second_prompt.tools));

    let equivalent_turn = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &router,
        equivalent_step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(equivalent_turn.locally_reused);
    assert!(Arc::ptr_eq(&second.scaffold, &equivalent_turn.scaffold));
    assert_eq!(
        cache.build_count(),
        1,
        "value-equivalent per-turn config owners must not rebuild stable request scaffolding"
    );

    let cold_second_prompt = build_projected_prompt(
        session.as_ref(),
        &second_prepared,
        &router,
        step_context.as_ref(),
        base_instructions.clone(),
    );
    assert_eq!(second_prompt.input, cold_second_prompt.input);
    assert_eq!(
        second_prompt.stable_context_fallback_input,
        cold_second_prompt.stable_context_fallback_input
    );
    assert_eq!(
        second_prompt.tool_history_fallback_input,
        cold_second_prompt.tool_history_fallback_input
    );
    assert_eq!(
        second_prompt.stable_context_tool_history_fallback_input,
        cold_second_prompt.stable_context_tool_history_fallback_input
    );
    assert_eq!(
        second_prompt.tool_history_substitutions,
        cold_second_prompt.tool_history_substitutions
    );
    assert_eq!(
        second_prompt.stable_context_fallback_tool_history_substitutions,
        cold_second_prompt.stable_context_fallback_tool_history_substitutions
    );
    assert_eq!(second_prompt.digests, cold_second_prompt.digests);
    assert_eq!(
        second_prompt.stable_context_manifest.projected_bytes(),
        cold_second_prompt.stable_context_manifest.projected_bytes()
    );
    assert_eq!(
        second_prompt.stable_context_manifest.projected_tokens(),
        cold_second_prompt
            .stable_context_manifest
            .projected_tokens()
    );
    assert!(stable_context_owner_matches(
        &second_prompt.stable_context_manifest,
        &cold_second_prompt.stable_context_manifest,
    ));
    assert_eq!(
        second_prompt.base_instructions,
        cold_second_prompt.base_instructions
    );
    assert_eq!(
        second_prompt.tools.serialized(),
        cold_second_prompt.tools.serialized()
    );
    assert_eq!(
        second_prompt.parallel_tool_calls,
        cold_second_prompt.parallel_tool_calls
    );
    assert_eq!(
        second_prompt.output_schema,
        cold_second_prompt.output_schema
    );
    assert_eq!(
        second_prompt.output_schema_strict,
        cold_second_prompt.output_schema_strict
    );
    let scaffold_measurements = crate::context::PromptContextBreakdown::from_response_items(
        &second_prompt.input,
        &second_prompt.prompt_provenance,
    )
    .expect("scaffold prompt provenance should measure")
    .measurements();
    let cold_measurements = crate::context::PromptContextBreakdown::from_response_items(
        &cold_second_prompt.input,
        &cold_second_prompt.prompt_provenance,
    )
    .expect("cold prompt provenance should measure")
    .measurements();
    assert_eq!(scaffold_measurements, cold_measurements);

    // Reconstructing an identical router does not change its model-visible
    // surface and must retain the same physical scaffold.
    let mut replacement_router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    ));
    let replaced_surface = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &replacement_router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(Arc::ptr_eq(&second.scaffold, &replaced_surface.scaffold));
    assert_eq!(cache.build_count(), 1);
    assert_eq!(
        second.scaffold.tools.serialized(),
        replaced_surface.scaffold.tools.serialized()
    );

    let changed_router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "changed_surface".to_string(),
                description: "genuinely changed tool schema".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::object(
                    Default::default(),
                    None,
                    Some(false.into()),
                ),
                output_schema: None,
            },
        )],
    );
    // Exercise replacement at the same address whenever ownership permits it.
    // A cache that retains only a raw address allows this in-place replacement
    // and incorrectly returns the old schema. Retained weak ownership requires
    // a fresh allocation instead, even after the previous router is released.
    if let Some(router) = Arc::get_mut(&mut replacement_router) {
        *router = changed_router;
    } else {
        replacement_router = Arc::new(changed_router);
    }
    let changed_surface = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &replacement_router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    let changed_prompt = build_projected_prompt_from_scaffold(
        &second_prepared,
        step_context.as_ref(),
        &changed_surface,
    );
    assert!(matches!(
        changed_prompt.tools.specs(),
        [codex_tools::ToolSpec::Function(tool)] if tool.name == "changed_surface"
    ));
    assert!(!Arc::ptr_eq(
        &replaced_surface.scaffold,
        &changed_surface.scaffold
    ));
    assert_eq!(cache.build_count(), 2);

    let changed_instructions = BaseInstructions {
        text: format!("{}\nchanged owner", base_instructions.text),
    };
    let replaced_instructions = cache.resolve(
        &second_prepared,
        session.as_ref(),
        &replacement_router,
        step_context.as_ref(),
        &changed_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(!Arc::ptr_eq(
        &changed_surface.scaffold,
        &replaced_instructions.scaffold
    ));
    assert_eq!(cache.build_count(), 3);
}

#[tokio::test]
async fn request_scaffold_reuses_terminal_and_ordinary_tool_surfaces() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "ordinary_surface".to_string(),
                description: "Visible only on ordinary generations.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            },
        )],
    ));
    let mut history = ContextManager::new();
    let user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "finish without tools when requested".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(
        [&user_message],
        turn_context.model_info.truncation_policy.into(),
    );
    let prepared = history.prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    let base_instructions = session.get_base_instructions().await;
    let mut cache = RequestScaffoldCache::default();

    let ordinary = cache.resolve(
        &prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(!ordinary.scaffold.tools.specs().is_empty());
    assert_eq!(
        ordinary.scaffold.digests.tools,
        Some(ordinary.scaffold.tools.digest())
    );

    let terminal = cache.resolve(
        &prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ true,
    );
    assert!(terminal.locally_reused);
    assert!(Arc::ptr_eq(
        &ordinary.scaffold.tools,
        &terminal.scaffold.tools
    ));
    assert_eq!(
        terminal.scaffold.digests.tools,
        Some(terminal.scaffold.tools.digest())
    );
    let mut terminal_prompt =
        build_projected_prompt_from_scaffold(&prepared, step_context.as_ref(), &terminal);
    enforce_terminal_prompt_contract(&mut terminal_prompt, true);
    assert!(!terminal_prompt.tools.specs().is_empty());
    assert!(terminal_prompt.tool_calls_disabled);
    assert_eq!(
        terminal_prompt.digests.tools,
        Some(terminal_prompt.tools.digest())
    );

    let ordinary_again = cache.resolve(
        &prepared,
        session.as_ref(),
        &router,
        step_context.as_ref(),
        &base_instructions,
        /*terminal_completion_only*/ false,
    );
    assert!(ordinary_again.locally_reused);
    assert!(!ordinary_again.scaffold.tools.specs().is_empty());
    assert_eq!(cache.build_count(), 1);
}

#[tokio::test]
async fn projected_prompt_defers_dynamic_history_measurement_across_retries() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    );
    let mut history = ContextManager::new();
    let user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "dynamic history is measured after dispatch".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(
        [&user_message],
        turn_context.model_info.truncation_policy.into(),
    );
    let prepared = history.prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    let prompt = build_projected_prompt(
        &session,
        &prepared,
        &router,
        step_context.as_ref(),
        session.get_base_instructions().await,
    );
    let retry_prompt = prompt.clone();

    assert_eq!(prompt.dynamic_history_measurement_count(), 0);
    assert!(
        prompt
            .stable_context_manifest
            .components()
            .iter()
            .all(|component| component.kind != StableContextKind::DynamicHistory)
    );

    let measured = prompt.measured_stable_context_manifest();
    assert_eq!(prompt.dynamic_history_measurement_count(), 1);
    assert!(
        measured
            .components()
            .iter()
            .any(|component| component.kind == StableContextKind::DynamicHistory)
    );
    let measured_retry = retry_prompt.measured_stable_context_manifest();
    assert_eq!(retry_prompt.dynamic_history_measurement_count(), 1);
    assert_eq!(measured.projected_bytes(), measured_retry.projected_bytes());
    assert_eq!(
        measured.projected_tokens(),
        measured_retry.projected_tokens()
    );
}

#[tokio::test]
async fn pending_turn_router_reuses_session_cache_until_planning_changes() -> Result<()> {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let first_step = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    // The first capture may publish the MCP runtime's selected scope.
    // Cache the generation belonging to that captured planning view.
    let planning_generation = session.services.planning_generation();
    let first = built_tools_for_pending_turn(
        session.as_ref(),
        &first_step,
        &[],
        planning_generation,
        &CancellationToken::new(),
    )
    .await?;
    let second_step = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let second = built_tools_for_pending_turn(
        session.as_ref(),
        &second_step,
        &[],
        planning_generation,
        &CancellationToken::new(),
    )
    .await?;
    assert!(Arc::ptr_eq(&first, &second));

    let changed_generation = {
        let mut state_owner = session.state.lock().await;
        session
            .services
            .advance_planning_generation(&mut state_owner)
    };
    let changed_step = session.capture_step_context(turn_context).await.unwrap();
    let changed = built_tools_for_pending_turn(
        session.as_ref(),
        &changed_step,
        &[],
        changed_generation,
        &CancellationToken::new(),
    )
    .await?;
    assert!(!Arc::ptr_eq(&second, &changed));
    let counters = changed_step
        .turn
        .turn_timing_state
        .complete_snapshot()
        .protocol_timing()
        .counters;
    assert_eq!(
        counters.tool_router_reuse_count, 0,
        "pending-turn planning reuse is not a model-generation router decision",
    );
    Ok(())
}

#[tokio::test]
async fn prepared_router_config_match_accepts_the_shared_turn_snapshot() {
    let (_session, turn_context) = crate::session::tests::make_session_and_context().await;
    let shared = Arc::clone(&turn_context.config);

    assert!(Arc::ptr_eq(&shared, &turn_context.config));
    assert!(same_config_snapshot(&shared, &turn_context.config));
    let mut distinct = Arc::new((*shared).clone());
    assert!(!Arc::ptr_eq(&shared, &distinct));
    assert!(same_config_snapshot(&shared, &distinct));
    Arc::make_mut(&mut distinct).service_tier = Some("changed-test-tier".to_string());
    assert!(!same_config_snapshot(&shared, &distinct));
}

#[tokio::test]
async fn prepared_router_rebuilds_for_same_slug_metadata_and_catalog_changes() {
    for startup in [false, true] {
        for changed in ["enable-search", "disable-search", "catalog"] {
            let (session, mut turn) = crate::session::tests::make_session_and_context().await;
            let session = Arc::new(session);
            turn.model_info.supports_search_tool = changed == "disable-search";
            let mut turn = Arc::new(turn);
            let step = session
                .capture_step_context(Arc::clone(&turn))
                .await
                .unwrap();
            // The first capture may publish the MCP runtime's selected scope.
            // Cache the generation belonging to that captured planning view.
            let generation = session.services.planning_generation();
            let cancellation = CancellationToken::new();
            let old = built_tools(&session, &step, &[], &cancellation)
                .await
                .unwrap();
            assert!(old.matches_planning_context(turn.as_ref()));
            if startup {
                session
                    .startup_prepared_router
                    .publish(crate::session_startup_prewarm::PreparedStartupRouter {
                        planning_generation: generation,
                        config: Arc::clone(&turn.config),
                        dynamic_tools: turn.dynamic_tools.clone(),
                        router: Arc::clone(&old),
                    })
                    .await;
            } else {
                session
                    .prepared_tool_router
                    .publish(generation, turn.as_ref(), Arc::clone(&old));
            }
            drop(step);
            let current = Arc::get_mut(&mut turn).expect("only the immutable plan was cached");
            if changed == "catalog" {
                Arc::make_mut(&mut current.available_models).push(
                    serde_json::from_value(serde_json::json!({
                        "id": "new-catalog-model", "model": "new-catalog-model",
                        "display_name": "New catalog model", "description": "Refreshed catalog",
                        "default_reasoning_effort": "medium", "supported_reasoning_efforts": [],
                        "is_default": false, "upgrade": null, "show_in_picker": true,
                        "availability_nux": null, "supported_in_api": true
                    }))
                    .unwrap(),
                );
            } else {
                current.model_info.supports_search_tool = changed == "enable-search";
            }
            assert!(!old.matches_planning_context(turn.as_ref()), "{changed}");
            let step = session
                .capture_step_context(Arc::clone(&turn))
                .await
                .unwrap();
            let rebuilt =
                built_tools_for_pending_turn(&session, &step, &[], generation, &cancellation)
                    .await
                    .unwrap();
            assert!(!Arc::ptr_eq(&old, &rebuilt), "{changed}, startup={startup}");
            assert!(rebuilt.matches_planning_context(turn.as_ref()));
            let warm =
                built_tools_for_pending_turn(&session, &step, &[], generation, &cancellation)
                    .await
                    .unwrap();
            assert!(
                Arc::ptr_eq(&rebuilt, &warm),
                "unchanged inputs must reuse the plan"
            );
        }
    }
}

#[tokio::test]
async fn effective_workspace_roots_are_cached_in_the_turn_context() {
    let (_session, turn_context) = crate::session::tests::make_session_and_context().await;
    let first = turn_context.effective_workspace_roots();
    let second = turn_context.effective_workspace_roots();

    assert!(!first.is_empty());
    assert_eq!(first, second);
    assert_eq!(first.as_ptr(), second.as_ptr());
    assert_eq!(
        first,
        turn_context.config.effective_workspace_roots().as_slice()
    );
}

#[tokio::test]
async fn built_tools_uses_the_revision_tagged_on_the_step_mcp_snapshot() -> Result<()> {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let step_context = session.capture_step_context(turn_context).await.unwrap();
    let snapshot_revision = step_context
        .mcp
        .manager()
        .tool_catalog_revision()
        .saturating_add(7);
    step_context
        .seed_mcp_tool_snapshot_for_test(snapshot_revision, Vec::new(), true)
        .await;

    let router = built_tools(
        session.as_ref(),
        &step_context,
        &[],
        &CancellationToken::new(),
    )
    .await?;

    assert_eq!(
        router.exposure_identity().mcp_tool_catalog_revision,
        snapshot_revision
    );
    assert!(router.exposure_identity().mcp_resources_available);
    Ok(())
}

#[tokio::test]
async fn sampling_prompt_workspace_capture_is_skipped_without_workspace_evidence() {
    let (_session, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let mut history = ContextManager::new();
    let user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "ordinary conversation".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&user_message], TruncationPolicy::Tokens(10_000));

    let _prepared =
        prepare_sampling_prompt_for_client(history, &turn_context, &git_workspace).await;

    assert_eq!(git_workspace.workspace_evidence_capture_count(), 0);
}

#[tokio::test]
async fn sampling_prompt_workspace_capture_is_skipped_for_non_workspace_code_mode_exec() {
    let (_session, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let mut history = ContextManager::new();
    let call_id = "non-workspace-code-mode";
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "functions.exec".to_string(),
        namespace: None,
        arguments: "await tools.list_mcp_resources({})".to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text("resources".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.register_non_workspace_code_mode_call(call_id.to_string());
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let _prepared =
        prepare_sampling_prompt_for_client(history, &turn_context, &git_workspace).await;

    assert_eq!(git_workspace.workspace_evidence_capture_count(), 0);
}

#[tokio::test]
async fn sampling_prompt_workspace_capture_is_preserved_for_workspace_evidence() {
    let (_session, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let mut history = ContextManager::new();
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "functions.exec".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"git status --short"}"#.to_string(),
        call_id: "workspace-call".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "workspace-call".to_string(),
        output: FunctionCallOutputPayload::from_text("clean".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let _prepared =
        prepare_sampling_prompt_for_client(history, &turn_context, &git_workspace).await;

    assert_eq!(git_workspace.workspace_evidence_capture_count(), 1);
}

#[tokio::test]
async fn token_backfire_deferred_tool_schema_survives_multiple_same_turn_requests() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let advertised = codex_tools::ToolName::plain("advertised_deferred");
    let discovered_during_request = codex_tools::ToolName::plain("newly_discovered_deferred");
    turn_context.refresh_deferred_tool_capabilities(Arc::new(HashMap::from([
        (advertised.clone(), "revision-a".to_string()),
        (discovered_during_request.clone(), "revision-b".to_string()),
    ])));
    turn_context.activate_deferred_tools(std::iter::once(advertised.clone()));
    let advertised_for_request = turn_context.activated_deferred_tools();

    turn_context.activate_deferred_tools(std::iter::once(discovered_during_request.clone()));
    turn_context.release_advertised_deferred_tools(&advertised_for_request);

    assert!(
        turn_context.deferred_tool_is_activated(&advertised),
        "an intervening same-turn request must not discard an activated callable schema"
    );
    assert!(
        turn_context.deferred_tool_is_activated(&discovered_during_request),
        "discovery performed during the request must remain visible to its continuation"
    );
}

#[tokio::test]
async fn deferred_tool_schema_survives_sampling_error_exit() {
    let (_, turn_context, _) = crate::session::tests::make_session_and_context_with_rx().await;
    let advertised = codex_tools::ToolName::plain("advertised_deferred");
    let discovered_during_request = codex_tools::ToolName::plain("discovered_during_request");
    turn_context.refresh_deferred_tool_capabilities(Arc::new(HashMap::from([
        (advertised.clone(), "revision-a".to_string()),
        (discovered_during_request.clone(), "revision-b".to_string()),
    ])));
    turn_context.activate_deferred_tools(std::iter::once(advertised.clone()));

    {
        let _lease = AdvertisedDeferredToolLease::new(
            Arc::clone(&turn_context),
            turn_context.activated_deferred_tools(),
        );
        turn_context.activate_deferred_tools(std::iter::once(discovered_during_request.clone()));
    }

    assert!(turn_context.deferred_tool_is_activated(&advertised));
    assert!(turn_context.deferred_tool_is_activated(&discovered_during_request));
}

#[tokio::test]
async fn deferred_activation_survives_transient_capability_absence() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let tool = codex_tools::ToolName::namespaced("mcp__calendar", "create_event");
    let available = Arc::new(HashMap::from([(tool.clone(), "calendar-v1".to_string())]));
    turn_context.refresh_deferred_tool_capabilities(Arc::clone(&available));
    turn_context.activate_deferred_tools(std::iter::once(tool.clone()));
    assert!(turn_context.deferred_tool_is_activated(&tool));

    // A router built from a timed-out MCP capture registers no MCP tools.
    turn_context.refresh_deferred_tool_capabilities(Arc::new(HashMap::new()));
    assert!(
        !turn_context.deferred_tool_is_activated(&tool),
        "an unregistered capability must never be treated as activated"
    );
    assert!(!turn_context.activated_deferred_tools().contains(&tool));

    // The next coherent capture registers the same contract again.
    turn_context.refresh_deferred_tool_capabilities(available);
    assert!(
        turn_context.deferred_tool_is_activated(&tool),
        "a transient absence must not revoke an activation whose revision is unchanged"
    );
    assert!(turn_context.activated_deferred_tools().contains(&tool));
}

#[tokio::test]
async fn deferred_activation_drops_on_changed_capability_revision() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let tool = codex_tools::ToolName::namespaced("mcp__calendar", "create_event");
    let original = Arc::new(HashMap::from([(tool.clone(), "calendar-v1".to_string())]));
    turn_context.refresh_deferred_tool_capabilities(Arc::clone(&original));
    turn_context.activate_deferred_tools(std::iter::once(tool.clone()));
    assert!(turn_context.deferred_tool_is_activated(&tool));

    // A changed contract is stale: the activation is removed, not parked.
    turn_context.refresh_deferred_tool_capabilities(Arc::new(HashMap::from([(
        tool.clone(),
        "calendar-v2".to_string(),
    )])));
    assert!(!turn_context.deferred_tool_is_activated(&tool));
    turn_context.refresh_deferred_tool_capabilities(original);
    assert!(
        !turn_context.deferred_tool_is_activated(&tool),
        "a revision change must discard the activation even if the old contract returns"
    );
}

#[tokio::test]
async fn mcp_discovery_completion_supersedes_only_a_delivered_notice() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let marker = "mcp_catalog_availability";
    // Negative control: a session never told discovery was incomplete hears nothing.
    assert!(
        !supersede_context_notice_if_present(&session, &turn, marker, MCP_DISCOVERY_COMPLETE_NOTICE)
            .await
            .unwrap()
    );
    assert!(retained_context_notices(session.clone_history().await.raw_items(), marker).is_empty());

    let incomplete = "MCP discovery is incomplete. Already advertised tools remain usable. Pending servers: [\"slow\"]. Failed or cancelled servers: []. A later step may discover more capabilities.";
    assert!(
        record_context_notice_if_changed(&session, &turn, marker, incomplete)
            .await
            .unwrap()
    );
    // Once every server settles, the stale notice is superseded exactly once.
    assert!(
        supersede_context_notice_if_present(&session, &turn, marker, MCP_DISCOVERY_COMPLETE_NOTICE)
            .await
            .unwrap()
    );
    assert!(
        !supersede_context_notice_if_present(&session, &turn, marker, MCP_DISCOVERY_COMPLETE_NOTICE)
            .await
            .unwrap()
    );
    assert_eq!(
        retained_context_notices(session.clone_history().await.raw_items(), marker),
        vec![
            incomplete.to_string(),
            MCP_DISCOVERY_COMPLETE_NOTICE.to_string()
        ]
    );
}

#[test]
fn audit_reports_17_19_retry_preserves_accepted_history_and_retry_limit() -> Result<()> {
    run_turn_multi_thread_test_with_stack("audit_reports_17_19_retry", || async {
        crate::test_harness::require_network!();
        for accepted_output in [false, true] {
            let server = responses::start_mock_server().await;
            let partial = if accepted_output {
                responses::sse(vec![responses::ev_assistant_message(
                    "partial",
                    "retained partial answer",
                )])
            } else {
                responses::sse(vec![])
            };
            let requests = responses::mount_sse_sequence(
                &server,
                vec![
                    partial,
                    responses::sse(vec![
                        responses::ev_assistant_message("final", "finished answer"),
                        responses::ev_completed("finished"),
                    ]),
                ],
            )
            .await;
            let test = test_codex()
                .with_config(|config| {
                    config.model_provider.stream_max_retries = Some(1);
                })
                .build(&server)
                .await?;
            let completion = test
                .submit_turn_and_capture_completion("finish this task")
                .await?;
            assert!(completion.error.is_none(), "{completion:?}");
            let requests = requests.requests();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                serde_json::to_string(&requests[1].input())
                    .unwrap()
                    .contains("retained partial answer"),
                accepted_output
            );
            if !accepted_output {
                assert_eq!(requests[0].input(), requests[1].input());
            }
        }
        Ok(())
    })
}





#[test]
fn kd4_latency_continuation_prefetch_rejects_stale_or_steered_state() {
    assert!(continuation_workspace_prefetch_is_current(7, 7, false));
    assert!(!continuation_workspace_prefetch_is_current(7, 8, false));
    assert!(!continuation_workspace_prefetch_is_current(7, 7, true));
}

#[tokio::test]
async fn local_compaction_reuses_pending_capture_and_rejects_stale_revision() {
    let (_session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).compact_prompt = Some("local summary".into());
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let revision = tracker.lock().await.current_mutation_revision();
    for stale in [false, true] {
        let mut pending = Some((
            revision,
            AbortOnDropHandle::new(tokio::spawn(async { None })),
        ));
        if stale {
            tracker.lock().await.record_unknown_mutation();
        }
        let mut identity = None;
        reuse_workspace_prefetch_for_local_compaction(
            &turn,
            &tracker,
            &mut pending,
            &mut identity,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(pending.is_none());
        // Some(None) preserves a completed non-Git capture, suppressing reprobes.
        assert_eq!(identity, if stale { None } else { Some(None) });
    }
}

#[tokio::test]
async fn local_compaction_pending_capture_is_cancellable() {
    let (_session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).compact_prompt = Some("local summary".into());
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let revision = tracker.lock().await.current_mutation_revision();
    let mut pending = Some((
        revision,
        AbortOnDropHandle::new(tokio::spawn(std::future::pending())),
    ));
    let mut identity = None;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        reuse_workspace_prefetch_for_local_compaction(
            &turn,
            &tracker,
            &mut pending,
            &mut identity,
            &cancellation,
        )
        .await,
        Err(CodexErr::TurnAborted)
    ));
    assert!(pending.is_none());
    assert!(identity.is_none());
}

#[tokio::test]
async fn kd4_latency_continuation_prefetch_skips_non_workspace_eager_read() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let mut history = ContextManager::new();
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "list_mcp_resources".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "non-workspace-read".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "non-workspace-read".to_string(),
        output: FunctionCallOutputPayload::from_text("resources".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let prefetch = start_continuation_workspace_prefetch(
        &history,
        &turn_diff_tracker,
        Arc::clone(&git_workspace),
        turn_context.config.cwd.clone(),
        turn_context.environments.clone(),
    )
    .await;

    assert!(prefetch.is_none());
    assert_eq!(git_workspace.workspace_evidence_capture_count(), 0);
}

#[tokio::test]
async fn continuation_prefetch_skips_non_workspace_code_mode_exec() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let mut history = ContextManager::new();
    let call_id = "non-workspace-code-mode-prefetch";
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "functions.exec".to_string(),
        namespace: None,
        arguments: "await tools.list_mcp_resources({})".to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text("resources".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.register_non_workspace_code_mode_call(call_id.to_string());
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let prefetch = start_continuation_workspace_prefetch(
        &history,
        &turn_diff_tracker,
        Arc::clone(&git_workspace),
        turn_context.config.cwd.clone(),
        turn_context.environments.clone(),
    )
    .await;

    assert!(prefetch.is_none());
    assert_eq!(git_workspace.workspace_evidence_capture_count(), 0);
}

#[tokio::test]
async fn kd4_latency_continuation_prefetch_preserves_workspace_evidence_read() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let git_workspace = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let mut history = ContextManager::new();
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "functions.exec".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"git status --short"}"#.to_string(),
        call_id: "workspace-read".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "workspace-read".to_string(),
        output: FunctionCallOutputPayload::from_text("clean".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let (_, handle) = start_continuation_workspace_prefetch(
        &history,
        &turn_diff_tracker,
        Arc::clone(&git_workspace),
        turn_context.config.cwd.clone(),
        turn_context.environments.clone(),
    )
    .await
    .expect("workspace evidence should start a continuation prefetch");
    let _ = handle
        .await
        .expect("continuation workspace prefetch should join");

    assert_eq!(git_workspace.workspace_evidence_capture_count(), 1);
}







#[test]
fn tool_relay_reconciliation_advances_without_watchdog() {
    let timing = TurnTimingState::default();
    timing.mark_turn_started();
    let mut orphan_passes = 0;

    timing.adjust_parallel_gate_waiters(1);
    assert_eq!(
        reconcile_turn_progress(&timing, 1, &mut orphan_passes),
        NextSampleBlockReason::WaitingForGate
    );
    timing.adjust_parallel_gate_waiters(-1);
    timing.adjust_relay_queue_depth(1);
    assert_eq!(
        reconcile_turn_progress(&timing, 1, &mut orphan_passes),
        NextSampleBlockReason::WaitingForDelivery
    );
    timing.adjust_relay_queue_depth(-1);
    timing.adjust_active_tools(1);
    assert_eq!(
        reconcile_turn_progress(&timing, 0, &mut orphan_passes),
        NextSampleBlockReason::WaitingForTool
    );
    assert_eq!(
        reconcile_turn_progress(&timing, 0, &mut orphan_passes),
        NextSampleBlockReason::WaitingForTool,
        "a retained or nested tool may remain active across reconciliation passes"
    );
    assert_eq!(orphan_passes, 2);
    timing.adjust_active_tools(-1);
    assert_eq!(
        reconcile_turn_progress(&timing, 0, &mut orphan_passes),
        NextSampleBlockReason::ReadyToSample
    );
    assert_eq!(orphan_passes, 0);
}

#[test]
fn ordered_sampling_prefix_holds_ready_state_until_append_completes() {
    assert_eq!(
        hold_sampling_readiness_for_ordered_prefix(NextSampleBlockReason::ReadyToSample),
        NextSampleBlockReason::WaitingForDelivery
    );
    assert_eq!(
        hold_sampling_readiness_for_ordered_prefix(NextSampleBlockReason::WaitingForGate),
        NextSampleBlockReason::WaitingForGate
    );
}

fn authoritative_wait_result(
    surfaceable_message: Option<&str>,
) -> crate::session::turn_execution::AuthoritativeWaitOwnerResult {
    crate::session::turn_execution::AuthoritativeWaitOwnerResult {
        adapter: "code_mode_cell".to_string(),
        value: serde_json::json!("arbitrary raw execution output"),
        surfaceable_message: surfaceable_message.map(ToOwned::to_owned),
    }
}

fn recommended_plugin_candidate(id: &str, name: &str) -> DiscoverableTool {
    codex_tools::DiscoverablePluginInfo {
        id: id.to_string(),
        remote_plugin_id: None,
        name: name.to_string(),
        description: None,
        has_skills: false,
        mcp_server_names: Vec::new(),
        app_connector_ids: Vec::new(),
    }
    .into()
}

#[test]
fn named_recommended_plugin_is_the_only_injected_candidate() {
    for (task, expected) in [
        ("fix the parser", vec![]),
        ("add a plugin field to the Rust parser", vec![]),
        ("use Figma for this mockup", vec!["Figma"]),
        ("use FIGMA and (notion)", vec!["Figma", "Notion"]),
        ("figmatic and notional changes", vec![]),
        ("éfigma notioné", vec![]),
        ("use design-tool", vec!["Figma"]),
        ("", vec![]),
    ] {
        let selected = task_relevant_recommended_plugins(
            &[ContentItem::InputText {
                text: task.to_string(),
            }],
            vec![
                recommended_plugin_candidate("design-tool", "Figma"),
                recommended_plugin_candidate("notion", "Notion"),
            ],
        );
        assert_eq!(
            selected
                .iter()
                .map(DiscoverableTool::name)
                .collect::<Vec<_>>(),
            expected,
            "{task}"
        );
    }
}

#[test]
fn recommended_plugin_catalog_is_bounded_by_rendered_bytes() {
    let candidates = (0..50)
        .map(|index| {
            recommended_plugin_candidate(
                &format!("plugin-{index}-{}", "x".repeat(180)),
                &format!("Plugin {index} {}", "y".repeat(180)),
            )
        })
        .collect();
    let instructions = RecommendedPluginsInstructions::from_plugins(candidates)
        .expect("at least one bounded plugin entry");
    let ResponseItem::Message { content, .. } = ContextualUserFragment::into(instructions) else {
        panic!("expected recommended plugin message");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected recommended plugin text");
    };

    assert!(text.len() <= 4_160);
    assert!(!text.contains("Plugin 49"));
}

#[test]
fn proven_loop_terminal_generation_ends_unless_new_input_arrives() {
    let request = GenerationRequestDisposition {
        purpose: Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning),
        sampling: SamplingGenerationDisposition::DecisionBearing,
        relevant_state_fingerprint: "state".to_string(),
        failure_fingerprint: None,
        terminal_completion_only: true,
    };

    assert!(!generation_needs_follow_up(&request, true, false));
    assert!(generation_needs_follow_up(&request, false, true));
}

#[test]
fn convergence_directive_is_consumed_before_compaction_can_replace_the_request() {
    let mut decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::ModelRequired,
        directive: Some("change strategy before continuing".to_string()),
        proven_loop_activated: true,
        authoritative_wait: None,
    };

    assert_eq!(
        take_convergence_observation(Some(&mut decision)),
        (true, Some("change strategy before continuing".to_string()))
    );
    assert_eq!(
        take_convergence_observation(Some(&mut decision)),
        (false, None)
    );
}

#[test]
fn compaction_rebases_but_preserves_a_terminal_completion_request() {
    let request = GenerationRequestDisposition {
        purpose: Some(TurnTimingGenerationPurpose::ValidationInterpretation),
        sampling: SamplingGenerationDisposition::DecisionBearing,
        relevant_state_fingerprint: "before-compaction".to_string(),
        failure_fingerprint: Some("failure".to_string()),
        terminal_completion_only: true,
    };

    let rebased =
        rebase_generation_request_after_compaction(request, "after-compaction".to_string());

    assert_eq!(
        rebased.purpose,
        Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning)
    );
    assert_eq!(
        rebased.sampling,
        SamplingGenerationDisposition::DecisionBearing
    );
    assert_eq!(rebased.relevant_state_fingerprint, "after-compaction");
    assert_eq!(rebased.failure_fingerprint.as_deref(), Some("failure"));
    assert!(rebased.terminal_completion_only);
}

#[test]
fn verified_turn_contract_after_agent_abort_revokes_delivery_authority() {
    let surfaced_result = SurfacedToolResult {
        adapter: "code_mode_cell".to_string(),
        value: serde_json::json!({"answer": 42}),
        canonical_message: Some("completed answer".to_string()),
    };

    let result = after_agent_abort_result(
        Some("completed answer".to_string()),
        Some(surfaced_result),
        true,
    );

    assert_eq!(
        result.last_agent_message.as_deref(),
        Some("completed answer")
    );
    assert_eq!(result.surfaced_result, None);
    assert!(result.required_tool_terminal.is_none());
    assert!(result.defer_pending_input);
}

#[test]
fn logical_generation_budget_allows_bounded_regular_and_one_terminal_generation() {
    let mut budget = LogicalGenerationBudget::default();
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            budget.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert_eq!(
        budget.admit(/*terminal_requested*/ false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
    assert_eq!(
        budget.admit(/*terminal_requested*/ false),
        LogicalGenerationAdmission::Exhausted
    );
}

#[test]
fn owned_polling_credits_are_capped_so_the_forced_synthesis_stays_reachable() {
    let mut budget = LogicalGenerationBudget::default();
    // Every generation only monitors owned processes; the first credits keep
    // the allowance, later ones spend it.
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS + MAX_MONITORING_GENERATION_CREDITS {
        assert_eq!(budget.admit(false), LogicalGenerationAdmission::Regular);
        budget.observe_progress(true);
    }
    assert_eq!(
        budget.admit(false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
    // New instructions renew the allowance and the credits.
    budget.accepted_user_input();
    for _ in 0..MAX_MONITORING_GENERATION_CREDITS {
        assert_eq!(budget.admit(false), LogicalGenerationAdmission::Regular);
        budget.observe_progress(true);
    }
    assert_eq!(budget.regular_generations, 0);
}

#[test]
fn only_mail_from_the_owner_renews_the_generation_budget() {
    let mail = |author: &str, recipient: &str| {
        TurnInput::InterAgentCommunication(codex_protocol::protocol::InterAgentCommunication::new(
            codex_protocol::AgentPath::try_from(author).unwrap(),
            codex_protocol::AgentPath::try_from(recipient).unwrap(),
            Vec::new(),
            "message".to_string(),
            true,
        ))
    };
    for (author, recipient, renews) in [
        ("/root/worker", "/root", false),
        ("/root/worker", "/root/reviewer", false),
        ("/root/worker", "/root/worker2", false),
        ("/root", "/root/worker", true),
        ("/root/worker", "/root/worker/helper", true),
    ] {
        let input = mail(author, recipient);
        assert!(
            resets_turn_execution(&input),
            "mail is still accepted context"
        );
        assert_eq!(
            renews_generation_budget(&input),
            renews,
            "{author} -> {recipient}"
        );
    }
}

#[test]
fn validation_failure_at_generation_limit_keeps_repair_tools_available() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "validation_failure_at_generation_limit_keeps_repair_tools_available",
        validation_failure_at_generation_limit_keeps_repair_tools_available_impl,
    )
}

async fn validation_failure_at_generation_limit_keeps_repair_tools_available_impl() -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    // The former 16-generation novelty cutoff must not suppress repair tools.
    let mut sequence = (0..15)
        .map(|index| {
            let mut completed = responses::ev_completed(&format!("thinking-{index}"));
            completed["response"]["end_turn"] = serde_json::json!(false);
            responses::sse(vec![completed])
        })
        .collect::<Vec<_>>();
    let python = if cfg!(windows) { "python" } else { "python3" };
    sequence.push(responses::sse(vec![
        responses::ev_shell_command_call(
            "validation-failed",
            &format!("{python} -c \"raise AssertionError('repair-required')\""),
        ),
        responses::ev_completed("failed"),
    ]));
    sequence.push(responses::sse(vec![
        responses::ev_apply_patch_custom_tool_call(
            "repair",
            "*** Begin Patch\n*** Add File: repaired.txt\n+fixed\n*** End Patch",
        ),
        responses::ev_completed("repaired"),
    ]));
    sequence.push(responses::sse(vec![
        responses::ev_shell_command_call("validated", &format!("{python} -c \"from pathlib import Path; assert Path('repaired.txt').read_text().strip() == 'fixed'; print('validation-passed')\"")),
        responses::ev_completed("validated"),
    ]));
    sequence.push(responses::sse(vec![
        responses::ev_assistant_message("answer", "Repair validated."),
        responses::ev_completed("finished"),
    ]));
    let requests = responses::mount_sse_sequence(&server, sequence).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Kd4Runtime).unwrap();
            config.features.disable(Feature::CodeModeHost).unwrap();
            config.features.disable(Feature::UnifiedExec).unwrap();
            config.model_auto_compact_token_limit = Some(i64::MAX);
        })
        .build(&server)
        .await?;
    let completion = test
        .submit_turn_and_capture_completion(
            "Validate, repair any failure, and validate the repair.",
        )
        .await?;
    assert!(completion.error.is_none(), "{completion:?}");
    assert_eq!(
        fs::read_to_string(test.workspace_path("repaired.txt"))?,
        "fixed\n"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("Repair validated.")
    );
    let sent = requests.requests();
    let limit = 16;
    assert_eq!(sent.len(), limit + 3);
    assert!(
        sent[limit]
            .function_call_output_text("validation-failed")
            .unwrap()
            .contains("repair-required")
    );
    assert!(
        sent[limit + 2]
            .function_call_output_text("validated")
            .unwrap()
            .contains("validation-passed")
    );
    for request in &sent[limit..] {
        assert!(!request.body_json()["tools"].as_array().unwrap().is_empty());
        assert!(!request.body_contains_text(LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE));
    }
    Ok(())
}

#[tokio::test]
async fn forced_terminal_budget_boundary_warns_without_changing_history() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;

    let history_before = session.clone_history().await;
    record_forced_terminal_budget_boundary(
        session.as_ref(),
        turn_context.as_ref(),
        ForcedTerminalReason::GenerationBudget,
    )
    .await;

    let history = session.clone_history().await;
    assert_eq!(history.raw_items(), history_before.raw_items());
    let warning = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("forced-terminal event channel remains open");
            if let EventMsg::Warning(warning) = event.msg {
                break warning;
            }
        }
    })
    .await
    .expect("forced-terminal boundary emits a warning");
    assert_eq!(
        warning.message,
        format!(
            "This turn reached its explicit safety budget of {MAX_REGULAR_LOGICAL_GENERATIONS} generations. Work is suspended; the assistant will report completed and unfinished work. Send another message to resume."
        )
    );
}

#[test]
fn completion_pending_input_stays_in_the_sampling_loop_when_capacity_remains() {
    let available = LogicalGenerationBudget::default();
    assert_eq!(
        completion_pending_input_disposition(available, true),
        CompletionPendingInputDisposition::Continue
    );
    assert_eq!(
        completion_pending_input_disposition(available, false),
        CompletionPendingInputDisposition::None
    );

    let mut no_regular_capacity = LogicalGenerationBudget::default();
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            no_regular_capacity.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert_eq!(
        completion_pending_input_disposition(no_regular_capacity, true),
        CompletionPendingInputDisposition::Defer
    );
}


#[tokio::test]
async fn convergence_telemetry_does_not_inject_model_advice() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut control = TurnExecutionControl::new();
    let mut decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::TerminalCompletionRequired,
        directive: Some("Report the remaining work.".to_string()),
        proven_loop_activated: true,
        authoritative_wait: None,
    };
    record_convergence_decision(&session, &turn_context, &mut control, Some(&mut decision))
        .await
        .unwrap();
    record_convergence_decision(&session, &turn_context, &mut control, Some(&mut decision))
        .await
        .unwrap();
    let mut advisory = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::ModelRequired,
        directive: Some("Try a different method.".to_string()),
        proven_loop_activated: false,
        authoritative_wait: None,
    };
    record_convergence_decision(&session, &turn_context, &mut control, Some(&mut advisory))
        .await
        .unwrap();
    let mut repeated_advisory = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::ModelRequired,
        directive: Some("Try a different method.".to_string()),
        proven_loop_activated: false,
        authoritative_wait: None,
    };
    record_convergence_decision(
        &session,
        &turn_context,
        &mut control,
        Some(&mut repeated_advisory),
    )
    .await
    .unwrap();
    assert!(session.clone_history().await.raw_items().is_empty());
    let mut next_turn = TurnExecutionControl::new();
    repeated_advisory.directive = Some("Try a different method.".to_string());
    record_convergence_decision(
        &session,
        &turn_context,
        &mut next_turn,
        Some(&mut repeated_advisory),
    )
    .await
    .unwrap();
    assert!(session.clone_history().await.raw_items().is_empty());
    let mut warnings = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::Warning(warning) = event.msg {
            warnings.push(warning.message);
        }
    }
    assert!(warnings.is_empty());
}

#[test]
fn logical_generation_budget_terminal_attempt_is_exactly_once() {
    let mut budget = LogicalGenerationBudget::default();
    assert_eq!(
        budget.admit(/*terminal_requested*/ true),
        LogicalGenerationAdmission::Terminal { forced: false }
    );
    assert_eq!(
        budget.admit(/*terminal_requested*/ true),
        LogicalGenerationAdmission::Exhausted
    );
}

#[test]
fn logical_generation_budget_new_user_input_renews_the_regular_limit() {
    let mut budget = LogicalGenerationBudget::default();
    for _ in 0..3 {
        assert_eq!(budget.admit(false), LogicalGenerationAdmission::Regular);
    }
    assert_eq!(
        budget.admit(true),
        LogicalGenerationAdmission::Terminal { forced: false }
    );
    assert!(!budget.can_admit(true));
    budget.accepted_user_input();
    assert!(budget.can_admit(true));
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(budget.admit(false), LogicalGenerationAdmission::Regular);
    }
    assert_eq!(
        budget.admit(false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
    assert_eq!(budget.admit(false), LogicalGenerationAdmission::Exhausted);
}

#[tokio::test]
async fn regular_follow_up_admission_reports_exhaustion_once() {
    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let available = LogicalGenerationBudget::default();
    let mut reported = false;
    assert!(admit_regular_follow_up(&session, &turn, &available, &mut reported).await);
    assert!(!reported);
    assert!(events.try_recv().is_err());

    reported = true;
    assert!(!admit_regular_follow_up(&session, &turn, &available, &mut reported).await);
    assert!(
        events.try_recv().is_err(),
        "an earlier failure must not emit a duplicate"
    );

    let exhausted = LogicalGenerationBudget {
        regular_generations: MAX_REGULAR_LOGICAL_GENERATIONS,
        terminal_generation_used: true,
        ..LogicalGenerationBudget::default()
    };
    reported = false;
    assert!(!admit_regular_follow_up(&session, &turn, &exhausted, &mut reported).await);
    assert!(reported);
    let EventMsg::Error(error) = events.try_recv().expect("budget error").msg else {
        panic!("expected budget error");
    };
    assert_eq!(
        error.message,
        format!(
            "This turn exhausted its explicit safety budget of {MAX_REGULAR_LOGICAL_GENERATIONS} generations and one final summary. Work is suspended before all requested work completed. Send another message to resume."
        )
    );
    assert!(error.affects_turn_status());
    assert_eq!(turn.terminal_error.lock().await.as_ref(), Some(&error));
    assert!(!admit_regular_follow_up(&session, &turn, &exhausted, &mut reported).await);
    report_logical_generation_budget_exhausted(&session, &turn, &mut reported).await;
    assert!(
        events.try_recv().is_err(),
        "budget error must be emitted once through admission and direct reporting"
    );
}

#[test]
fn logical_generation_budget_preview_preserves_capacity() {
    for (regular_generations, terminal_generation_used, regular_allowed, terminal_allowed) in [
        (0, false, true, true),
        (0, true, true, false),
        (MAX_REGULAR_LOGICAL_GENERATIONS, false, true, true),
        (MAX_REGULAR_LOGICAL_GENERATIONS, true, false, false),
    ] {
        let budget = LogicalGenerationBudget {
            regular_generations,
            terminal_generation_used,
            ..LogicalGenerationBudget::default()
        };
        assert_eq!(budget.can_admit(false), regular_allowed);
        assert_eq!(budget.can_admit(true), terminal_allowed);
        assert_eq!(budget.regular_generations, regular_generations);
        assert_eq!(budget.terminal_generation_used, terminal_generation_used);
    }
}

#[test]
fn used_terminal_then_regular_generation_limit_blocks_follow_up_before_input_drain() {
    let mut budget = LogicalGenerationBudget::default();
    assert_eq!(
        budget.admit(/*terminal_requested*/ true),
        LogicalGenerationAdmission::Terminal { forced: false }
    );
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            budget.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    let next_request = GenerationRequestDisposition {
        purpose: Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning),
        sampling: SamplingGenerationDisposition::DecisionBearing,
        relevant_state_fingerprint: "budget-exhausted".to_string(),
        failure_fingerprint: None,
        terminal_completion_only: false,
    };

    assert!(budget.is_exhausted());
    assert!(generation_budget_blocks_follow_up(
        budget,
        Some(&next_request)
    ));
}

#[tokio::test]
async fn terminal_generation_boundary_does_not_poll_or_drain_pending_input() {
    let mut budget = LogicalGenerationBudget::default();
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            budget.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert!(!budget.is_exhausted());

    let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let future_polled = Arc::clone(&polled);
    let drained = drain_pending_input_if_generation_available(&budget, async move {
        future_polled.store(true, Ordering::Release);
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "must remain queued for a fresh turn".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }]
    })
    .await;

    assert_eq!(drained, Some(Vec::new()));
    assert!(!polled.load(Ordering::Acquire));
    assert_eq!(
        budget.admit(/*terminal_requested*/ false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
}

#[tokio::test]
async fn exhausted_generation_budget_does_not_poll_or_drain_pending_input() {
    let mut budget = LogicalGenerationBudget::default();
    for _ in 0..MAX_REGULAR_LOGICAL_GENERATIONS {
        assert_eq!(
            budget.admit(/*terminal_requested*/ false),
            LogicalGenerationAdmission::Regular
        );
    }
    assert_eq!(
        budget.admit(/*terminal_requested*/ false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
    assert!(budget.is_exhausted());

    let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let future_polled = Arc::clone(&polled);
    let drained = drain_pending_input_if_generation_available(&budget, async move {
        future_polled.store(true, Ordering::Release);
        vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "must remain queued".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }]
    })
    .await;

    assert!(drained.is_none());
    assert!(!polled.load(Ordering::Acquire));
}

#[tokio::test]
async fn untyped_status_affecting_error_is_recorded_as_terminal() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let error = ErrorEvent {
        message: "untyped terminal error".to_string(),
        codex_error_info: None,
    };
    assert!(error.affects_turn_status());

    session
        .send_event(turn_context.as_ref(), EventMsg::Error(error.clone()))
        .await;

    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("untyped terminal error emits an event")
        .expect("event channel remains open");
    let EventMsg::Error(emitted_error) = event.msg else {
        panic!("expected untyped terminal error event");
    };
    assert_eq!(emitted_error, error);
    assert_eq!(
        turn_context.terminal_error.lock().await.as_ref(),
        Some(&error)
    );
}

#[tokio::test]
async fn planning_failure_records_initial_input_and_emits_status_affecting_error() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "preserve this initial prompt".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: Some("planning-failure-input".to_string()),
    }];

    let failure = planning_failure("injected test failure");
    assert!(
        !failure.is_retryable(),
        "local planning errors must not retry the transport"
    );
    let result = finish_pending_turn_planning_failure(&session, &turn_context, &input, failure)
        .await
        .expect("planning failure is surfaced as a terminal turn result");
    assert!(
        result.defer_pending_input,
        "pending input must not re-enter a turn whose planning failed"
    );

    let error = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("planning failure event channel remains open");
            if let EventMsg::Error(error) = event.msg {
                break error;
            }
        }
    })
    .await
    .expect("planning failure emits an error event");
    assert!(error.message.contains("pending-turn planning failure"));
    assert_eq!(error.codex_error_info, Some(CodexErrorInfo::Other));
    assert!(error.affects_turn_status());
    assert_eq!(
        turn_context.terminal_error.lock().await.as_ref(),
        Some(&error)
    );

    let history = session.clone_history().await;
    assert!(history.raw_items().iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { role, content, .. }
                if role == "user"
                    && content.iter().any(|item| matches!(
                        item,
                        ContentItem::InputText { text }
                            if text == "preserve this initial prompt"
                    ))
        )
    }));
}

#[tokio::test]
async fn lightweight_terminal_prompt_contract_preserves_tools_and_disables_calls() {
    let (_, turn_context) = crate::session::tests::make_session_and_context().await;
    let registry = ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let router = ToolRouter::from_parts(
        registry,
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "read_only_probe".to_string(),
                description: "probe".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::object(
                    Default::default(),
                    None,
                    Some(false.into()),
                ),
                output_schema: None,
            },
        )],
    );
    let mut prompt = build_prompt(
        Vec::<ResponseItem>::new(),
        &router,
        &turn_context,
        BaseInstructions::default(),
    );
    assert!(!prompt.tools.specs().is_empty());
    let tools = Arc::clone(&prompt.tools);
    prompt.parallel_tool_calls = true;

    enforce_terminal_prompt_contract(&mut prompt, /*terminal_completion_only*/ true);

    assert!(Arc::ptr_eq(&prompt.tools, &tools));
    assert!(prompt.parallel_tool_calls);
    assert!(prompt.tool_calls_disabled);
    enforce_terminal_prompt_contract(&mut prompt, /*terminal_completion_only*/ false);
    assert!(!prompt.tool_calls_disabled);
}

#[tokio::test]
async fn sampling_context_notices_are_persisted_once_and_append_only() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    assert!(
        !record_context_notice_if_changed(&session, &turn, "tool_search_sources", "")
            .await
            .unwrap()
    );
    assert!(
        record_context_notice_if_changed(&session, &turn, "tool_search_sources", "- docs")
            .await
            .unwrap()
    );
    let first = session.clone_history().await.raw_items().to_vec();
    assert!(
        !record_context_notice_if_changed(&session, &turn, "tool_search_sources", "- docs")
            .await
            .unwrap()
    );
    assert_eq!(session.clone_history().await.raw_items(), first.as_slice());
    for sources in ["- docs\n- browser", "- docs", ""] {
        let before = session.clone_history().await.raw_items().to_vec();
        assert!(
            record_context_notice_if_changed(&session, &turn, "tool_search_sources", sources)
                .await
                .unwrap()
        );
        let history = session.clone_history().await;
        assert!(history.raw_items().starts_with(&before));
        assert_eq!(history.raw_items().len(), before.len() + 1);
    }
    // Root tasks get no phrase-triggered full-read policy.
    record_sampling_notices(&session, &turn, false, /*forced_terminal*/ None)
        .await
        .unwrap();
    assert!(
        !serde_json::to_string(session.clone_history().await.raw_items())
            .unwrap()
            .contains("<exhaustive_scope_feasibility>")
    );
    record_sampling_notices(
        &session,
        &turn,
        true,
        Some(ForcedTerminalReason::GenerationBudget),
    )
    .await
    .unwrap();
    let notices = session.clone_history().await.raw_items().to_vec();
    for _ in 0..2 {
        // A recorded notice is neither duplicated nor retracted by later steps.
        record_sampling_notices(
            &session,
            &turn,
            true,
            Some(ForcedTerminalReason::GenerationBudget),
        )
        .await
        .unwrap();
        assert_eq!(
            session.clone_history().await.raw_items(),
            notices.as_slice()
        );
    }
    let rendered = serde_json::to_string(&notices).unwrap();
    assert!(!rendered.contains("<exhaustive_scope_feasibility>"));
    assert_eq!(rendered.matches("<mcp_catalog_notice>").count(), 1);
    assert_eq!(rendered.matches("<forced_terminal_notice>").count(), 1);
    assert!(rendered.contains("Later user input may resume work"));
}

#[tokio::test]
async fn root_scope_phrases_preserve_requests_without_injecting_notices() {
    let long_narrow_request = format!(
        "Fix one README typo. {} No full audit is authorized.",
        "Do not change the scope. ".repeat(40)
    );
    for request in [
        "Fix the typo only; do not conduct a full review.",
        "Do not read every file. Inspect src/parser.rs only.",
        "The log says \"full audit failed\"; explain this one error.",
        "Rename the UI label \"Complete Review\" to \"Review Complete\" in labels.json.",
        "Find the definition of full_review; do not run an exhaustive review.",
        "The previous request to review the entire repository is cancelled. Fix only the typo in README.md.",
        long_narrow_request.as_str(),
        "Explain how a full audit differs from a sampled audit; do not inspect repository files.",
        // Positive controls: the host must retain the complete requested scope
        // without asking to narrow it or inserting a feasibility inventory.
        "Read every file in src/ and list the unsafe blocks.",
        "Do an exhaustive review of the parser.",
        "Audit the entire codebase for panics.",
        "Read the rollout in full before answering.",
        "Compare all of the sessions line by line.",
        "Review all changed files.",
        "A full-read audit of each source file, please.",
        "Go through everything under scripts/.",
    ] {
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let input = vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: request.to_owned(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }];
        record_inspected_inputs(&session, &turn, &input)
            .await
            .unwrap();
        record_sampling_notices(&session, &turn, false, None)
            .await
            .unwrap();
        let history = session.clone_history().await;
        assert_eq!(history.raw_items().len(), 1, "{request}");
        let prepared = prepare_sampling_prompt_with_workspace_identity(
            history,
            &turn,
            None,
            session.services.git_workspace.as_ref(),
        );
        assert_eq!(prepared.items().len(), 1, "{request}");
        let ResponseItem::Message { role, content, .. } = &prepared.items()[0] else {
            panic!("accepted request must remain a message");
        };
        assert_eq!(role, "user");
        assert_eq!(
            content,
            &vec![ContentItem::InputText {
                text: request.to_owned()
            }]
        );
    }
}

#[tokio::test]
async fn accepted_user_messages_survive_input_packing_without_proof_journal() {
    let texts = ["Do not implement; audit only.", "Include exact source line references."];
    for initial_input in [true, false] {
        for separate_messages in [true, false] {
            let (session, turn, _events) = crate::session::tests::make_session_and_context_with_rx().await;
            let parts = texts.map(|text| UserInput::Text { text: text.into(), text_elements: Vec::new() });
            let input = if separate_messages {
                parts.into_iter().map(|part| TurnInput::UserInput { content: vec![part], client_id: None }).collect()
            } else {
                vec![TurnInput::UserInput { content: parts.to_vec(), client_id: None }]
            };
            if initial_input {
                record_inspected_inputs(&session, &turn, &input).await.unwrap();
            } else {
                let outcome = run_hooks_and_record_inputs_detailed(&session, &turn, input).await.unwrap();
                assert!(outcome.accepted_context_input);
                assert!(!outcome.should_stop);
            }
            let history = session.clone_history().await;
            let text = serde_json::to_string(history.raw_items()).unwrap();
            for original in texts {
                assert_eq!(text.matches(original).count(), 1);
            }
            assert!(!text.contains("accepted_input_refs"));
        }
    }
}

#[tokio::test]
async fn rejected_broad_sibling_cannot_supply_scope_or_root_notice() {
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;
    use codex_config::ConfigLayerStack;
    use codex_config::ConfigRequirements;
    use codex_config::ConfigRequirementsToml;

    let (session, turn, _events) = crate::session::tests::make_session_and_context_with_rx().await;
    let script = "import json, sys; blocked = 'comprehensive' in json.load(sys.stdin)['prompt']; sys.stderr.write('broad input rejected' if blocked else ''); sys.exit(2 if blocked else 0)";
    let layer = serde_json::from_value(serde_json::json!({"hooks": {
        "UserPromptSubmit": [{"hooks": [{
            "type": "command",
            "command": format!("python3 -c \"{script}\""),
            "commandWindows": format!("python -c \"{script}\""),
        }]}]
    }}))
    .unwrap();
    let stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: turn.config.codex_home.join("config.toml"),
                profile: None,
            },
            layer,
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .unwrap();
    session
        .services
        .hooks
        .store(Arc::new(codex_hooks::Hooks::new(
            codex_hooks::HooksConfig {
                feature_enabled: true,
                bypass_hook_trust: true,
                config_layer_stack: Some(stack),
                ..Default::default()
            },
        )));
    let store = &session.services.plan_store;
    store
        .update_tool(
            serde_json::from_value(serde_json::json!({
                "plan": [{"step": "Fix typo", "status": "pending"}]
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let input =
        ["Perform a comprehensive audit.", "Fix this one typo."].map(|text| TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: text.into(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        });
    let outcome = run_hooks_and_record_inputs_detailed(&session, &turn, input.to_vec())
        .await
        .unwrap();
    assert!(!outcome.should_stop);
    assert!(outcome.accepted_context_input);
    record_sampling_notices(&session, &turn, false, None)
        .await
        .unwrap();
    let prepared = prepare_sampling_prompt_with_workspace_identity(
        session.clone_history().await,
        &turn,
        None,
        session.services.git_workspace.as_ref(),
    );
    let rendered = serde_json::to_string(prepared.items()).unwrap();
    assert!(rendered.contains("Fix this one typo."));
    assert!(!rendered.contains("Perform a comprehensive audit."));
    assert!(!rendered.contains("<exhaustive_scope_feasibility>"));
}

#[tokio::test]
async fn spawned_agent_notice_preserves_budget_without_a_scope_classifier() {
    use codex_protocol::protocol::SubAgentSource;

    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: codex_protocol::ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let limit = turn
        .model_info
        .auto_compact_token_limit()
        .or_else(|| turn.model_context_window())
        .expect("the test model has a context limit");
    // Illustrate three quarters of the compaction limit at 4 bytes a token.
    let megabytes = (limit * 3 / 4 * 4) as f64 / 1_000_000.0;
    let child = spawned_agent_context_budget_notice(&turn);
    assert!(child.starts_with("Before committing to an exhaustive review or full-read request"));
    assert!(child.contains(&format!("~{megabytes:.1} MB uses three quarters")));
    assert!(child.contains("Rough inline capacity illustration, not a timing estimate"));
    assert!(child.contains("retiring exactly recoverable consumed output"));
    assert!(child.contains("model summarization that can lose detail"));
    // The removed token-budget window reset is no longer a relief route.
    assert!(!child.contains("fresh-window"));
    assert!(child.contains("duration is unknown without measurements for the active route"));
    assert!(!child.contains("forces a compaction"));
    assert!(!child.contains("minutes each"));

    assert!(child.contains(&format!(
        "Your context budget is {MAX_SUBAGENT_CONTEXT_PRESSURE_EVENTS} counted context-pressure events per turn"
    )));
    assert!(child.contains("duration is unknown without measurements for the active route"));
    assert!(child.contains("unread files or byte ranges"));
    assert!(
        !child.contains("autoResolutionMs"),
        "a spawned agent does not ask the user"
    );
    assert!(!child.contains("Continue reading through compactions"));
    for _ in 0..2 {
        record_sampling_notices(&session, &turn, true, None)
            .await
            .unwrap();
    }
    for marker in ["tool_search_sources", "mcp_catalog_availability", "mcp_catalog_notice"] {
        assert!(!record_context_notice_if_changed(&session, &turn, marker, "catalog").await.unwrap());
    }
    let rendered = serde_json::to_string(session.clone_history().await.raw_items()).unwrap();
    assert!(!rendered.contains("tool_search_sources"));
    assert!(!rendered.contains("mcp_catalog"));
    assert_eq!(
        rendered.matches("<exhaustive_scope_feasibility>").count(),
        1
    );
    assert!(rendered.contains(&child));
}

#[tokio::test]
async fn spawned_agent_notice_does_not_invent_unknown_capacity() {
    let (_session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.model_info.context_window = None;
    turn.model_info.max_context_window = None;
    turn.model_info.auto_compact_token_limit = None;
    let notice = spawned_agent_context_budget_notice(&turn);
    assert!(notice.contains("model context limit is unknown"));
    assert!(!notice.contains("MB uses three quarters"));
    assert!(notice.contains("duration is unknown without measurements for the active route"));
    assert!(notice.contains("Report unfinished coverage in the final answer"));
}

#[test]
fn spawned_agent_context_budget_forces_one_final_synthesis() {
    use codex_protocol::protocol::SubAgentSource;

    let child = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: codex_protocol::ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let mut events = 0;
    let mut budget = LogicalGenerationBudget::default();
    assert!(!observe_context_pressure_event(
        &child,
        &mut events,
        &mut budget
    ));
    assert_eq!(budget.admit(false), LogicalGenerationAdmission::Regular);
    assert!(observe_context_pressure_event(
        &child,
        &mut events,
        &mut budget
    ));
    assert_eq!(
        budget.admit(false),
        LogicalGenerationAdmission::Terminal { forced: true }
    );
    assert_eq!(budget.admit(false), LogicalGenerationAdmission::Exhausted);

    // A root session keeps compacting; the budget is for spawned agents only.
    let mut root_events = 0;
    let mut root_budget = LogicalGenerationBudget::default();
    for _ in 0..5 {
        assert!(!observe_context_pressure_event(
            &SessionSource::Cli,
            &mut root_events,
            &mut root_budget,
        ));
    }
    assert_eq!(
        root_budget.admit(false),
        LogicalGenerationAdmission::Regular
    );
}

#[test]
fn spawned_agent_limit_retirements_leave_the_compaction_budget() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "spawned_agent_limit_retirements_leave_the_compaction_budget",
        spawned_agent_limit_retirements_leave_the_compaction_budget_impl,
    )
}

/// A spawned agent is told its budget is compactions, which lose detail.
/// Retirement at the limit keeps every output exactly recoverable, like the
/// uncounted proactive retirement it now usually replaces, so two of them in
/// one turn must not force the agent to stop and report unfinished coverage.
async fn spawned_agent_limit_retirements_leave_the_compaction_budget_impl() -> Result<()> {
    use codex_protocol::protocol::SubAgentSource;

    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let over_limit_continuation = |id: &str| {
        responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_function_call(&format!("{id}-call"), "unknown_test_tool", "{}"),
            responses::ev_completed_with_tokens(id, 100_500),
        ])
    };
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            over_limit_continuation("first"),
            over_limit_continuation("second"),
            responses::sse(vec![
                responses::ev_response_created("final"),
                responses::ev_assistant_message("final-message", "assigned scope covered"),
                responses::ev_completed_with_tokens("final", 1_000),
            ]),
        ],
    )
    .await;
    let home = tempfile::tempdir()?;
    let provider = non_openai_model_provider(&server);
    let (session, mut turn, _events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            move |config| {
                config.model_provider = provider;
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(0);
                config.model_auto_compact_token_limit = Some(100_000);
                config.model_auto_compact_token_limit_scope =
                    codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
                config.features.disable(Feature::CodeModeHost).unwrap();
            },
        )
        .await;
    Arc::get_mut(&mut turn)
        .expect("turn context fixture is uniquely owned")
        .session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: codex_protocol::ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    // Enough consumed results for both limit retirements, while the projected
    // first prompt stays under the limit so nothing retires before the turn.
    seed_retirable_consumed_outputs(&session, 50).await;
    // Pending-input recording belongs to an admitted task for this turn.
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            SignalCompletingTask {
                finish: CancellationToken::new(),
            },
        )
        .await;

    let mut budget = LogicalGenerationBudget::default();
    tokio::time::timeout(
        Duration::from_secs(30),
        run_turn(
            Arc::clone(&session),
            Arc::clone(&turn),
            Arc::new(ExtensionData::new(turn.sub_id.clone())),
            vec![TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "read the assigned files".into(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            }],
            None,
            &mut budget,
            CancellationToken::new(),
        ),
    )
    .await??;

    let retirements = session
        .state
        .lock()
        .await
        .history
        .raw_items()
        .iter()
        .filter(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => {
                content.iter().any(|part| {
                    matches!(
                        part,
                        codex_protocol::models::ContentItem::InputText { text }
                            if crate::tool_history::is_pressure_retirement_checkpoint_text(text)
                    )
                })
            }
            _ => false,
        })
        .count();
    assert_eq!(
        retirements, 2,
        "each over-limit continuation retires consumed results instead of summarizing"
    );
    let sent = requests.requests();
    assert_eq!(
        sent.len(),
        3,
        "both continuations and the final answer are sampled"
    );
    assert!(
        !sent[2].body_contains_text("reached its context budget"),
        "retirement at the limit must not force the final synthesis"
    );
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
    Ok(())
}

#[test]
fn usage_limit_waits_for_an_account_switch_and_retries_the_same_request() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "usage_limit_waits_for_an_account_switch_and_retries_the_same_request",
        || async {
            let server = responses::start_mock_server().await;
            let usage_limit = ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "error": {
                    "type": "usage_limit_reached",
                    "message": "limit reached",
                    "plan_type": "pro"
                }
            }));
            let requests = responses::mount_response_sequence(
                &server,
                vec![
                    usage_limit,
                    responses::sse_response(responses::sse(vec![
                        responses::ev_assistant_message("answer", "after the switch"),
                        responses::ev_completed("done"),
                    ])),
                ],
            )
            .await;
            let home = tempfile::tempdir()?;
            let provider = non_openai_model_provider(&server);
            let (mut session, mut turn, events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    CodexAuth::from_api_key("test key"),
                    Vec::new(),
                    home.path(),
                    move |config| {
                        config.model_provider = provider;
                        // The retry must not come from the stream retry budget.
                        config.model_provider.stream_max_retries = Some(0);
                        config.model_provider.request_max_retries = Some(0);
                        config.features.disable(Feature::CodeModeHost).unwrap();
                    },
                )
                .await;
            // The account an interactive turn waits on. Its stored credentials
            // match the cached ones, as after a normal login.
            let auth_home = tempfile::tempdir()?;
            let login = |api_key: &str| {
                codex_login::login_with_api_key(
                    auth_home.path(),
                    api_key,
                    codex_login::AuthCredentialsStoreMode::File,
                    codex_login::AuthKeyringBackendKind::default(),
                )
            };
            login("first-account")?;
            let auth_manager = AuthManager::from_auth_for_testing_with_home(
                CodexAuth::from_api_key("first-account"),
                auth_home.path().to_path_buf(),
            );
            {
                let turn = Arc::get_mut(&mut turn).expect("test turn context is uniquely owned");
                turn.provider = codex_model_provider::create_model_provider(
                    turn.provider.info().clone(),
                    Some(Arc::clone(&auth_manager)),
                );
                turn.auth_manager = Some(Arc::clone(&auth_manager));
                turn.session_source = SessionSource::Cli;
            }
            {
                // Waiting and dispatch must observe the same account owner;
                // swapping only turn.provider leaves the HTTP client unchanged.
                let session = Arc::get_mut(&mut session).expect("unique test session");
                session.services.auth_manager = Arc::clone(&auth_manager);
                session.services.model_client = crate::client::ModelClient::new(
                    Some(Arc::clone(&auth_manager)),
                    codex_login::auth::AgentIdentityAuthPolicy::JwtOnly,
                    session.thread_id,
                    turn.provider.info().clone(),
                    SessionSource::Cli,
                    "test_originator".to_string(),
                    turn.config.model_verbosity,
                    false,
                    false,
                    None,
                    false,
                    None,
                    turn.config.http_client_factory(),
                );
            }
            // Pending-input recording belongs to an admitted task for this turn.
            session
                .spawn_task(
                    Arc::clone(&turn),
                    Vec::new(),
                    SignalCompletingTask {
                        finish: CancellationToken::new(),
                    },
                )
                .await;
            let mut budget = LogicalGenerationBudget::default();
            // Joined futures move on the stack; keep the turn future on the heap.
            let run = Box::pin(run_turn(
                Arc::clone(&session),
                Arc::clone(&turn),
                Arc::new(ExtensionData::new(turn.sub_id.clone())),
                vec![TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "continue the task".into(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
                None,
                &mut budget,
                CancellationToken::new(),
            ));
            let switch_account = async {
                let warning = loop {
                    if let EventMsg::Warning(warning) = events.recv().await?.msg
                        && warning.message.contains("for an account switch")
                    {
                        break warning;
                    }
                };
                login("second-account")?;
                // `reload` reports a change of auth mode only; the switch still
                // advances the auth revision the turn waits on.
                auth_manager.reload().await;
                assert_eq!(
                    auth_manager
                        .auth_cached()
                        .and_then(|auth| auth.api_key().map(str::to_string)),
                    Some("second-account".to_string())
                );
                anyhow::Ok(warning)
            };
            let (result, warning) = tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(run, switch_account)
            })
            .await
            .expect("the account switch must end the wait long before its bound");
            let warning = warning?;
            assert!(
                warning.message.contains("usage limit"),
                "{}",
                warning.message
            );
            assert!(
                warning
                    .message
                    .contains("interrupt the turn to stop waiting")
            );
            let result = result?;
            assert_eq!(
                result.last_agent_message.as_deref(),
                Some("after the switch")
            );
            let sent = requests.requests();
            assert_eq!(sent.len(), 2, "one rejected request and one retry");
            assert_eq!(
                sent[0].header("authorization").as_deref(),
                Some("Bearer first-account"),
                "the rejected request uses the original account"
            );
            assert_eq!(
                sent[1].header("authorization").as_deref(),
                Some("Bearer second-account"),
                "the retry must use the switched account, not just wake on its notification"
            );
            assert_eq!(
                sent[0].body_json()["input"],
                sent[1].body_json()["input"],
                "the switch retries the same request"
            );
            session
                .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
                .await;
            Ok(())
        },
    )
}

#[test]
fn authoritative_wait_terminal_surface_requires_explicit_owner_projection() {
    let without_projection = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            authoritative_wait_result(None),
        )),
        ..Default::default()
    };
    assert_eq!(
        authoritative_wait_terminal_surface(&without_projection, None, 10_000),
        Some(SurfacedToolResult {
            adapter: "code_mode_cell".to_string(),
            value: serde_json::json!("arbitrary raw execution output"),
            canonical_message: None,
        }),
        "raw code-mode output must not become last_agent_message"
    );

    let with_projection = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            authoritative_wait_result(Some("owner-designated completion")),
        )),
        ..Default::default()
    };
    assert_eq!(
        authoritative_wait_terminal_surface(&with_projection, None, 10_000),
        Some(SurfacedToolResult {
            adapter: "code_mode_cell".to_string(),
            value: serde_json::json!("arbitrary raw execution output"),
            canonical_message: Some("owner-designated completion".to_string()),
        })
    );
}

#[test]
fn blocked_authoritative_wait_never_enters_terminal_surface() {
    for continuation in [
        ContinuationDisposition::ModelRequired,
        ContinuationDisposition::SurfaceExistingResult,
    ] {
        let blocked = SamplingConvergenceDecision {
            continuation,
            authoritative_wait: Some(AuthoritativeWaitResolution::Blocked(
                authoritative_wait_result(Some("must not surface")),
            )),
            ..Default::default()
        };
        assert_eq!(
            authoritative_wait_terminal_surface(&blocked, None, 10_000),
            None
        );
    }
}

fn run_turn_multi_thread_test_with_stack<F, Fut, T>(test_name: &'static str, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("turn test runtime")
                .block_on(test())
        })
        .expect("turn test thread")
        .join()
        .expect("turn test thread panicked")
}

struct RewriteAgentMessageContributor;

struct TurnInputBudgetContributor {
    text: String,
}

struct EnvironmentEchoContributor {
    input_addresses: std::sync::Mutex<Vec<usize>>,
}

struct CountingTurnInputContributor {
    poll_count: Arc<AtomicUsize>,
}

struct ExposureOnlyTool {
    name: &'static str,
    exposure: codex_extension_api::ToolExposure,
}

impl codex_extension_api::ToolExecutor<codex_extension_api::ToolCall> for ExposureOnlyTool {
    fn tool_name(&self) -> codex_extension_api::ToolName {
        codex_extension_api::ToolName::plain(self.name)
    }

    fn spec(&self) -> codex_extension_api::ToolSpec {
        panic!("exposure identity tests do not build tool schemas")
    }

    fn exposure(&self) -> codex_extension_api::ToolExposure {
        self.exposure
    }

    fn handle(
        &self,
        _call: codex_extension_api::ToolCall,
    ) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async { panic!("exposure identity tests do not dispatch tools") })
    }
}

#[derive(Clone)]
struct SignalCompletingTask {
    finish: CancellationToken,
}

impl SessionTask for SignalCompletingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.phase_68_signal_completing"
    }

    fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            tokio::select! {
                _ = self.finish.cancelled() => {}
                _ = cancellation_token.cancelled() => {}
            }
            Ok(crate::tasks::TurnTaskResult::default())
        })
    }
}

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

impl TurnInputContributor for TurnInputBudgetContributor {
    fn contribute<'a>(
        &'a self,
        _input: &'a TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        Box::pin(async move {
            vec![
                Box::new(codex_context_fragments::RenderedContextFragment::new(
                    "user",
                    self.text.clone(),
                )) as Box<dyn codex_extension_api::ContextualUserFragment + Send>,
            ]
        })
    }
}

impl TurnInputContributor for EnvironmentEchoContributor {
    fn contribute<'a>(
        &'a self,
        input: &'a TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        Box::pin(async move {
            self.input_addresses
                .lock()
                .expect("input addresses lock")
                .push(std::ptr::from_ref(input) as usize);
            let environment = input
                .environments
                .first()
                .expect("primary turn environment should reach contributors");
            vec![
                Box::new(codex_context_fragments::RenderedContextFragment::new(
                    "user",
                    format!(
                        "extension-environment:{}:{}:{}",
                        environment.environment_id, environment.cwd, environment.is_primary
                    ),
                )) as Box<dyn codex_extension_api::ContextualUserFragment + Send>,
            ]
        })
    }
}

impl TurnInputContributor for CountingTurnInputContributor {
    fn contribute<'a>(
        &'a self,
        _input: &'a TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        self.poll_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Vec::new() })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn ordinary_continuation_precedence_is_stable() {
    assert_eq!(
        ordinary_continuation_cause(true, true, true),
        Some(ContinuationCause::ToolResult)
    );
    assert_eq!(
        ordinary_continuation_cause(false, true, true),
        Some(ContinuationCause::ServerEndTurnFalse)
    );
    assert_eq!(
        ordinary_continuation_cause(false, false, true),
        Some(ContinuationCause::PendingInput)
    );
    assert_eq!(ordinary_continuation_cause(false, false, false), None);
}

#[tokio::test]
async fn finalized_router_reuse_rejects_changed_dynamic_exposure() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let current = current_dynamic_tool_exposure_identity(&session, step_context.as_ref()).await;
    let matching_identity = ToolExposureIdentity {
        agent_surface_stage: current.agent_surface_stage,
        extension_tool_surface_revision: current.extension_tool_surface_revision,
        mcp_tool_catalog_revision: current.mcp_tool_catalog_revision,
        mcp_resources_available: current.mcp_resources_available,
        request_user_input_eligible: current.request_user_input_eligible,
        collaboration_mode: current.collaboration_mode,
        environment_mode: current.environment_mode,
        environment_starting: current.environment_starting,
        windows_shell_guidance: current.windows_shell_guidance,
        native_environment_contract: current.native_environment_contract,
        ..ToolExposureIdentity::default()
    };
    let matching_router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        matching_identity.clone(),
    )
    .with_planning_context(turn.as_ref());
    assert!(
        finalized_router_matches_current_exposure(
            &session,
            step_context.as_ref(),
            &matching_router,
        )
        .await
    );

    for stale_identity in [
        ToolExposureIdentity {
            environment_mode: if matching_identity.environment_mode == EnvironmentSurfaceMode::One {
                EnvironmentSurfaceMode::None
            } else {
                EnvironmentSurfaceMode::One
            },
            ..matching_identity.clone()
        },
        ToolExposureIdentity {
            environment_starting: !matching_identity.environment_starting,
            ..matching_identity.clone()
        },
        ToolExposureIdentity {
            windows_shell_guidance: !matching_identity.windows_shell_guidance,
            ..matching_identity.clone()
        },
        ToolExposureIdentity {
            mcp_tool_catalog_revision: matching_identity.mcp_tool_catalog_revision + 1,
            ..matching_identity.clone()
        },
        ToolExposureIdentity {
            extension_tool_surface_revision: matching_identity.extension_tool_surface_revision + 1,
            ..matching_identity.clone()
        },
        ToolExposureIdentity {
            mcp_resources_available: !matching_identity.mcp_resources_available,
            ..matching_identity.clone()
        },
    ] {
        let stale = ToolRouter::from_parts_with_warnings_and_identity(
            ToolRegistry::empty_for_test(),
            Vec::new(),
            Vec::new(),
            stale_identity,
        )
        .with_planning_context(turn.as_ref());
        assert!(
            !finalized_router_matches_current_exposure(&session, step_context.as_ref(), &stale)
                .await
        );
    }

    let stale_router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        ToolExposureIdentity {
            request_user_input_eligible: !matching_identity.request_user_input_eligible,
            ..matching_identity.clone()
        },
    )
    .with_planning_context(turn.as_ref());
    assert!(
        !finalized_router_matches_current_exposure(&session, step_context.as_ref(), &stale_router,)
            .await
    );

    let stale_collaboration_mode_router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        ToolExposureIdentity {
            collaboration_mode: ModeKind::Plan,
            ..matching_identity
        },
    )
    .with_planning_context(turn.as_ref());
    assert!(
        !finalized_router_matches_current_exposure(
            &session,
            step_context.as_ref(),
            &stale_collaboration_mode_router,
        )
        .await
    );
}

#[test]
fn goal_surface_state_has_disabled_inactive_and_active_transitions() {
    let tool = |name, exposure| {
        Arc::new(ExposureOnlyTool { name, exposure })
            as Arc<dyn codex_extension_api::ToolExecutor<codex_extension_api::ToolCall>>
    };

    assert_eq!(goal_surface_state(&[]), GoalSurfaceState::Disabled);
    assert_eq!(
        goal_surface_state(&[tool(
            "create_goal",
            codex_extension_api::ToolExposure::Deferred,
        )]),
        GoalSurfaceState::Inactive
    );
    assert_eq!(
        goal_surface_state(&[
            tool("create_goal", codex_extension_api::ToolExposure::Deferred,),
            tool("get_goal", codex_extension_api::ToolExposure::Direct),
            tool("update_goal", codex_extension_api::ToolExposure::Direct),
        ]),
        GoalSurfaceState::Active
    );
}

#[tokio::test]
async fn agent_surface_stage_depends_only_on_coarse_graph_and_binding_state() {
    assert_eq!(
        agent_surface_stage_from_snapshot(false, false, false),
        AgentSurfaceStage::Prohibited
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, false, false),
        AgentSurfaceStage::SpawnOnly
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(false, true, false),
        AgentSurfaceStage::Lifecycle
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(false, false, true),
        AgentSurfaceStage::TypedAdministration
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, true, false),
        AgentSurfaceStage::Lifecycle
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, false, true),
        AgentSurfaceStage::TypedAdministration
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, true, true),
        AgentSurfaceStage::TypedAdministration
    );
    // Running/waiting status, gates, targets, and capacity are deliberately absent from the
    // snapshot signature, so those fine-grained transitions cannot change the schema identity.
    // The graph argument is the latched "ever had a live agent" flag rather than the current
    // graph, so a tree that empties keeps its surface and the cached tool prefix stays stable;
    // see `ever_had_live_agents_latches_across_a_drained_graph`.

    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.multi_agent_version = codex_protocol::protocol::MultiAgentVersion::V2;
    for policy in [
        None,
        Some(""),
        Some("Do not use subagents"),
        Some("Use subagents"),
    ] {
        Arc::make_mut(&mut turn.config)
            .multi_agent_v2
            .multi_agent_mode_hint_text = policy.map(str::to_string);
        assert_eq!(
            agent_surface_stage(&session, &turn),
            AgentSurfaceStage::SpawnOnly,
            "tool availability must not depend on parsing instruction text"
        );
    }
    turn.multi_agent_version = codex_protocol::protocol::MultiAgentVersion::Disabled;
    assert_eq!(
        agent_surface_stage(&session, &turn),
        AgentSurfaceStage::Prohibited
    );
}

fn response_input_texts(items: &[ResponseItem]) -> Vec<&str> {
    let mut texts = Vec::new();
    for item in items {
        if let ResponseItem::Message { content, .. } = item {
            for content_item in content {
                if let ContentItem::InputText { text } = content_item {
                    texts.push(text.as_str());
                }
            }
        }
    }
    texts
}

#[test]
fn turn_execution_resets_for_every_accepted_context_change() {
    let user_input = TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "new instruction".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    };
    let empty_user_input = TurnInput::UserInput {
        content: Vec::new(),
        client_id: None,
    };
    let response_item = TurnInput::ResponseItem(assistant_output_text("context"));
    let internal_response_item =
        TurnInput::InternalResponseItem(assistant_output_text("internal context"));
    let mailbox_item = TurnInput::InterAgentCommunication(InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "mailbox context".to_string(),
        true,
    ));

    assert!(resets_turn_execution(&user_input));
    assert!(!resets_turn_execution(&empty_user_input));
    assert!(resets_turn_execution(&response_item));
    assert!(!resets_turn_execution(&internal_response_item));
    assert!(resets_turn_execution(&mailbox_item));
}

#[test]
fn legacy_explicit_skill_items_share_one_hard_budget() {
    let max_bytes = codex_utils_string::approx_bytes_for_tokens(
        codex_context_fragments::MAX_MODEL_CONTEXT_TOKENS,
    );
    let skill = |name: &str, contents: String| codex_core_skills::injection::SkillInjection {
        source_id: None,
        name: name.to_string(),
        path: format!("/skills/{name}/SKILL.md"),
        contents,
        scope: codex_protocol::protocol::SkillScope::User,
    };
    let items = build_bounded_skill_context_items(&[
        skill(
            "first",
            format!(
                "{} DO NOT PUBLISH {}",
                "x".repeat(max_bytes / 2),
                "y".repeat(max_bytes / 2)
            ),
        ),
        skill("second", "legacy-skill-budget-second".to_string()),
    ]);
    let texts = response_input_texts(&items);

    assert!(texts.iter().map(|text| text.len()).sum::<usize>() <= max_bytes);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("INCOMPLETE MODEL DELIVERY")
                && text.contains("/skills/first/SKILL.md"))
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("legacy-skill-budget-second"))
    );
}

#[test]
fn legacy_skill_truncated_at_budget_edge_stays_contextual() {
    let max_bytes = codex_utils_string::approx_bytes_for_tokens(
        codex_context_fragments::MAX_MODEL_CONTEXT_TOKENS,
    );
    let skill = |name: &str, contents: String| codex_core_skills::injection::SkillInjection {
        source_id: None,
        name: name.to_string(),
        path: format!("/skills/{name}/SKILL.md"),
        contents,
        scope: codex_protocol::protocol::SkillScope::User,
    };
    // Reserve the later recovery notice before admitting earlier instructions.
    let first_overhead = skill("first", String::new()).render().len();
    let first = skill("first", "x".repeat(max_bytes - first_overhead - 20));
    let second = skill("second", "second skill instructions".to_string());

    let items = build_bounded_skill_context_items([&first, &second]);

    assert_eq!(items.len(), 2);
    assert!(
        response_input_texts(&items)
            .iter()
            .any(|text| text.contains("INCOMPLETE MODEL DELIVERY"))
    );
    assert!(
        response_input_texts(&items)
            .iter()
            .map(|text| text.len())
            .sum::<usize>()
            <= max_bytes
    );
    for item in &items {
        let ResponseItem::Message { role, content, .. } = item else {
            panic!("expected skill message, got {item:?}");
        };
        assert_eq!(role, "user");
        // Unrecognized user-role text would surface as user input and a turn boundary.
        assert!(
            crate::event_mapping::is_contextual_user_message_content(content),
            "{content:?}"
        );
    }
}

#[tokio::test]
async fn extension_turn_input_contributors_share_one_hard_budget() {
    let max_bytes = codex_utils_string::approx_bytes_for_tokens(
        codex_context_fragments::MAX_MODEL_CONTEXT_TOKENS,
    );
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_input_contributor(Arc::new(TurnInputBudgetContributor {
        text: format!("turn-input-budget-first:{}", "x".repeat(max_bytes)),
    }));
    builder.turn_input_contributor(Arc::new(TurnInputBudgetContributor {
        text: "turn-input-budget-second".to_string(),
    }));
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let step_context = StepContext::for_test(Arc::new(turn_context));

    let items =
        build_extension_turn_input_items(&session, &step_context, &[], &CancellationToken::new())
            .await
            .expect("turn-input contributors should render");
    let texts = response_input_texts(&items);

    assert!(texts.iter().map(|text| text.len()).sum::<usize>() <= max_bytes);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("INCOMPLETE MODEL DELIVERY") && text.contains("artifact_id"))
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("turn-input-budget-second"))
    );
}

#[tokio::test]
async fn extension_turn_input_contributors_receive_foreign_environment_uris() {
    let foreign_cwd = PathUri::parse("file:///usr/local/project").expect("POSIX cwd URI");
    assert!(
        foreign_cwd.to_abs_path().is_err(),
        "test cwd must be foreign to the host"
    );

    let (mut session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let environment = turn_context.environments.turn_environments[0].clone();
    turn_context.environments.turn_environments[0] =
        crate::session::turn_context::TurnEnvironment::new(
            "remote".to_string(),
            environment.environment,
            foreign_cwd.clone(),
            environment.shell,
        );
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    let contributor = Arc::new(EnvironmentEchoContributor {
        input_addresses: std::sync::Mutex::new(Vec::new()),
    });
    builder.turn_input_contributor(contributor.clone());
    builder.turn_input_contributor(contributor.clone());
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let step_context = StepContext::for_test(Arc::new(turn_context));

    let items =
        build_extension_turn_input_items(&session, &step_context, &[], &CancellationToken::new())
            .await
            .expect("foreign environment should render through extension context");
    let texts = response_input_texts(&items);

    assert_eq!(
        texts,
        vec![format!("extension-environment:remote:{foreign_cwd}:true"); 2]
    );
    let addresses = contributor
        .input_addresses
        .lock()
        .expect("input addresses lock");
    assert_eq!(addresses.len(), 2);
    assert_eq!(
        addresses[0], addresses[1],
        "contributors must share one input snapshot"
    );
}

#[test]
fn streamed_item_with_empty_id_gets_a_generated_id() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "streamed_item_with_empty_id_gets_a_generated_id",
        streamed_item_with_empty_id_gets_a_generated_id_impl,
    )
}

async fn streamed_item_with_empty_id_gets_a_generated_id_impl() -> Result<()> {
    crate::test_harness::require_network!();
    for provider_id in ["", "msg_sctx_provider", "ordinary-provider-id"] {
        assert_streamed_item_id(provider_id).await?;
    }
    Ok(())
}

async fn assert_streamed_item_id(provider_id: &str) -> Result<()> {
    let server = responses::start_mock_server().await;
    let test = test_codex().build(&server).await?;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("response-1"),
            responses::ev_message_item_added(provider_id, ""),
            responses::ev_output_text_delta("streamed"),
            responses::ev_assistant_message(provider_id, "streamed"),
            responses::ev_completed("response-1"),
        ]),
    )
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                // Implementation requests must stream and complete on the working
                // model's response, without a separate completion assessment.
                text: "Fix the timer.".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started_id = crate::test_harness::wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemStarted(event) => match &event.item {
            TurnItem::AgentMessage(item) => Some(item.id.clone()),
            _ => None,
        },
        _ => None,
    })
    .await;
    let delta = crate::test_harness::wait_for_event_match(&test.codex, |event| match event {
        EventMsg::AgentMessageContentDelta(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(delta.item_id, started_id);
    assert_eq!(delta.delta, "streamed");
    let completed_id =
        crate::test_harness::wait_for_event_match(&test.codex, |event| match event {
            EventMsg::ItemCompleted(event) => match &event.item {
                TurnItem::AgentMessage(item) => Some(item.id.clone()),
                _ => None,
            },
            _ => None,
        })
        .await;

    match provider_id {
        "" => assert!(started_id.starts_with("msg_")),
        "msg_sctx_provider" => assert_eq!(started_id, "msg_provider_msg_sctx_provider"),
        id => assert_eq!(started_id, id),
    }
    assert_eq!(started_id, completed_id);
    let completed = crate::test_harness::wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnComplete(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(completed.last_agent_message.as_deref(), Some("streamed"));
    response_mock.single_request();
    Ok(())
}

fn non_openai_model_provider(server: &wiremock::MockServer) -> ModelProviderInfo {
    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    // OpenAI identity is the trusted registration kind, not the display name.
    provider.kind = codex_model_provider_info::ProviderKind::Custom;
    provider.name = "OpenAI (phase 68 test)".to_string();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider
}

#[test]
fn first_turn_dispatch_aborts_never_completing_prewarm() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "first_turn_dispatch_aborts_never_completing_prewarm",
        || async {
            let server = responses::start_mock_server().await;
            let requests = responses::mount_sse_sequence(
                &server,
                vec![responses::sse(vec![
                    responses::ev_assistant_message("answer", "ordinary dispatch"),
                    responses::ev_completed("done"),
                ])],
            )
            .await;
            let home = tempfile::tempdir()?;
            let provider = non_openai_model_provider(&server);
            let (session, turn, _events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    CodexAuth::from_api_key("test key"),
                    Vec::new(),
                    home.path(),
                    move |config| {
                        config.model_provider = provider;
                        config.features.disable(Feature::CodeModeHost).unwrap();
                    },
                )
                .await;
            // Pending-input recording belongs to an admitted task for this turn.
            session
                .spawn_task(
                    Arc::clone(&turn),
                    Vec::new(),
                    SignalCompletingTask {
                        finish: CancellationToken::new(),
                    },
                )
                .await;
            let task = tokio::spawn(std::future::pending());
            let abort = task.abort_handle();
            session
                .set_session_startup_prewarm(
                    crate::session_startup_prewarm::SessionStartupPrewarmHandle::new(
                        task,
                        std::time::Instant::now(),
                    ),
                )
                .await;
            let mut budget = LogicalGenerationBudget::default();
            tokio::time::timeout(
                Duration::from_secs(10),
                run_turn(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    Arc::new(ExtensionData::new(turn.sub_id.clone())),
                    vec![TurnInput::UserInput {
                        content: vec![UserInput::Text {
                            text: "dispatch now".into(),
                            text_elements: Vec::new(),
                        }],
                        client_id: None,
                    }],
                    None,
                    &mut budget,
                    CancellationToken::new(),
                ),
            )
            .await
            .expect("first model request must not wait for speculative work")?;
            assert_eq!(requests.requests().len(), 1);
            assert!(requests.requests()[0].body_contains_text("dispatch now"));
            tokio::time::timeout(Duration::from_secs(1), async {
                while !abort.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert!(session.take_session_startup_prewarm().await.is_none());
            session
                .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
                .await;
            Ok(())
        },
    )
}

#[test]
fn refused_missing_answer_repair_reports_only_the_budget_cause() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "refused_missing_answer_repair_reports_only_the_budget_cause",
        || async {
            let server = responses::start_mock_server().await;
            let requests = responses::mount_sse_sequence(
                &server,
                vec![responses::sse(vec![
                    responses::ev_response_created("empty"),
                    responses::ev_completed("empty"),
                ])],
            )
            .await;
            let home = tempfile::tempdir()?;
            let provider = non_openai_model_provider(&server);
            let (session, turn, events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    CodexAuth::from_api_key("test key"),
                    Vec::new(),
                    home.path(),
                    move |config| {
                        config.model_provider = provider;
                        config.features.disable(Feature::CodeModeHost).unwrap();
                    },
                )
                .await;
            // Pending-input recording belongs to an admitted task for this turn.
            session
                .spawn_task(
                    Arc::clone(&turn),
                    Vec::new(),
                    SignalCompletingTask {
                        finish: CancellationToken::new(),
                    },
                )
                .await;
            // The sampled generation is the last regular one and the forced final
            // summary is already spent, so the missing-answer repair is refused.
            let mut budget = LogicalGenerationBudget {
                regular_generations: MAX_REGULAR_LOGICAL_GENERATIONS - 1,
                terminal_generation_used: true,
                monitoring_credits: 0,
            };
            tokio::time::timeout(
                Duration::from_secs(30),
                run_turn(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    Arc::new(ExtensionData::new(turn.sub_id.clone())),
                    Vec::new(),
                    None,
                    &mut budget,
                    CancellationToken::new(),
                ),
            )
            .await??;
            assert_eq!(
                requests.requests().len(),
                1,
                "a refused repair sends nothing"
            );
            let budget_message = format!(
                "This turn exhausted its explicit safety budget of {MAX_REGULAR_LOGICAL_GENERATIONS} generations and one final summary. Work is suspended before all requested work completed. Send another message to resume."
            );
            let mut errors = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let EventMsg::Error(error) = event.msg
                    && error.affects_turn_status()
                {
                    errors.push(error.message);
                }
            }
            assert_eq!(errors, vec![budget_message.clone()]);
            assert_eq!(
                turn.terminal_error
                    .lock()
                    .await
                    .as_ref()
                    .map(|error| error.message.clone()),
                Some(budget_message),
                "the budget refusal remains the terminal cause"
            );
            session
                .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
                .await;
            Ok(())
        },
    )
}

#[test]
fn first_turn_dispatch_reports_disabled_startup_prewarm() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "first_turn_dispatch_reports_disabled_startup_prewarm",
        || async {
            let server = responses::start_mock_server().await;
            let requests = responses::mount_sse_sequence(
                &server,
                vec![responses::sse(vec![
                    responses::ev_assistant_message("answer", "ordinary dispatch"),
                    responses::ev_completed("done"),
                ])],
            )
            .await;
            let home = tempfile::tempdir()?;
            let provider = non_openai_model_provider(&server);
            let (session, turn, _events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    CodexAuth::from_api_key("test key"),
                    Vec::new(),
                    home.path(),
                    move |config| {
                        config.model_provider = provider;
                        config.features.disable(Feature::CodeModeHost).unwrap();
                    },
                )
                .await;
            // Pending-input recording belongs to an admitted task for this turn.
            session
                .spawn_task(
                    Arc::clone(&turn),
                    Vec::new(),
                    SignalCompletingTask {
                        finish: CancellationToken::new(),
                    },
                )
                .await;
            // Tool preparation completed without a speculative model request.
            let task = tokio::spawn(async { Ok(None) });
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
            session
                .set_session_startup_prewarm(
                    crate::session_startup_prewarm::SessionStartupPrewarmHandle::new(
                        task,
                        std::time::Instant::now(),
                    ),
                )
                .await;
            let mut budget = LogicalGenerationBudget::default();
            tokio::time::timeout(
                Duration::from_secs(10),
                run_turn(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    Arc::new(ExtensionData::new(turn.sub_id.clone())),
                    vec![TurnInput::UserInput {
                        content: vec![UserInput::Text {
                            text: "dispatch now".into(),
                            text_elements: Vec::new(),
                        }],
                        client_id: None,
                    }],
                    None,
                    &mut budget,
                    CancellationToken::new(),
                ),
            )
            .await??;
            assert_eq!(requests.requests().len(), 1);
            assert!(requests.requests()[0].body_contains_text("dispatch now"));
            let snapshot = session.startup_timing.complete_snapshot();
            assert_eq!(snapshot.prewarm_status.as_deref(), Some("disabled"));
            assert_eq!(snapshot.phases.transport_preconnect_ns, 0);
            assert_eq!(snapshot.phases.prewarm_request_ns, 0);
            assert!(session.take_session_startup_prewarm().await.is_none());
            session
                .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
                .await;
            Ok(())
        },
    )
}



#[test]
fn generation_budget_renews_on_reentry_input_and_persists_terminal_directive() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "generation_budget_renews_on_reentry_input_and_persists_terminal_directive",
        generation_budget_renews_on_reentry_input_and_persists_terminal_directive_impl,
    )
}

async fn generation_budget_renews_on_reentry_input_and_persists_terminal_directive_impl()
-> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let limit = MAX_REGULAR_LOGICAL_GENERATIONS as usize;
    let mut sequence = (0..limit - 2)
        .map(|i| {
            let id = format!("regular-{i}");
            let mut completed = responses::ev_completed(&id);
            completed["response"]["end_turn"] = serde_json::json!(false);
            responses::sse(vec![responses::ev_response_created(&id), completed])
        })
        .collect::<Vec<_>>();
    sequence.push(responses::sse(vec![
        responses::ev_assistant_message("reentry", "ready for reentry"),
        responses::ev_completed("reentry"),
    ]));
    for i in 0..limit {
        let mut after_reentry = responses::ev_completed(&format!("after-reentry-{i}"));
        after_reentry["response"]["end_turn"] = serde_json::json!(false);
        sequence.push(responses::sse(vec![after_reentry]));
    }
    for id in ["terminal", "next-turn"] {
        sequence.push(responses::sse(vec![
            responses::ev_assistant_message(id, id),
            responses::ev_completed(id),
        ]));
    }
    let requests = responses::mount_sse_sequence(&server, sequence).await;
    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            let ready =
                serde_json::to_string(&home.join("reentry.ready").to_string_lossy()).unwrap();
            let release =
                serde_json::to_string(&home.join("reentry.release").to_string_lossy()).unwrap();
            let script_path = home.join("reentry_hook.py");
            fs::write(
                &script_path,
                format!(
                    r#"import json, sys, time
from pathlib import Path
json.load(sys.stdin)
ready, release = Path({ready}), Path({release})
if not ready.exists():
    ready.write_text("ready")
    deadline = time.monotonic() + 60
    while not release.exists():
        if time.monotonic() >= deadline:
            raise RuntimeError("reentry input was never queued")
        time.sleep(0.01)
    print(json.dumps({{"continue": False, "stopReason": "reenter queued input"}}))
else:
    print("{{}}")
"#
                ),
            )
            .unwrap();
            fs::write(
                home.join("hooks.json"),
                serde_json::json!({
                    "hooks": {"Stop": [{"hooks": [{
                        "type": "command",
                        "command": format!("python3 \"{}\"", script_path.display()),
                        "commandWindows": format!("python \"{}\"", script_path.display()),
                    }]}]}
                })
                .to_string(),
            )
            .unwrap();
        })
        .with_config(|config| {
            trust_discovered_hooks(config);
            config.model_auto_compact_token_limit = Some(i64::MAX);
            // Exercise the hard task budget without an earlier convergence
            // decision terminating this intentionally repetitive fixture.
            config
                .features
                .disable(Feature::Kd4Runtime)
                .expect("disable the separate convergence policy");
        });
    let test = builder.build(&server).await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "keep working".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(60), async {
        while !test.codex_home_path().join("reentry.ready").exists() {
            // Consume the normal client event stream while the long turn runs.
            // Otherwise its bounded queue can stall before the reentry hook.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                event = test.codex.next_event() => {
                    let event = event.expect("event stream remains open before reentry");
                    assert!(
                        !matches!(event.msg, EventMsg::TurnComplete(_) | EventMsg::Error(_)),
                        "turn ended before the reentry boundary: {:?}",
                        event.msg
                    );
                }
            }
        }
    })
    .await?;
    assert_eq!(
        requests.requests().len(),
        limit - 1,
        "reentry follows all but one regular generation"
    );
    // Queue input while Stop holds the first run_turn at its completion boundary.
    // The hook ends that invocation, so RegularTask must drain and reenter it.
    test.codex
        .steer_input(
            vec![UserInput::Text {
                text: "queued reentry input".to_string(),
                text_elements: Vec::new(),
            }],
            Default::default(),
            None,
            None,
            None,
        )
        .await
        .expect("steer input is accepted by the active regular task");
    fs::write(test.codex_home_path().join("reentry.release"), "release")?;
    // The renewed allowance runs another full set of generations, so this wait
    // gets the same budget as the first phase.
    let completion = crate::test_harness::wait_for_event_match_with_timeout(
        &test.codex,
        |event| match event {
            EventMsg::TurnComplete(event) => Some(event.clone()),
            _ => None,
        },
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(completion.last_agent_message.as_deref(), Some("terminal"));
    assert!(
        completion.error.is_some(),
        "budget suspension must not report success"
    );
    assert_eq!(
        requests.requests().len(),
        2 * limit,
        "accepted reentry input renews the bounded allowance before one terminal request"
    );
    assert!(requests.requests()[limit - 1].body_contains_text("queued reentry input"));

    test.submit_turn("start a fresh turn").await?;
    let sent = requests.requests();
    assert_eq!(
        sent.len(),
        2 * limit + 1,
        "the terminal request cannot trigger another generation"
    );
    for (index, request) in sent.iter().enumerate() {
        let terminal = index == 2 * limit - 1;
        assert_eq!(
            request.body_contains_text(LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE),
            index >= 2 * limit - 1,
            "the terminal instruction must remain in history after the forced request"
        );
        assert_eq!(
            request.body_json()["tool_choice"],
            if terminal { "none" } else { "auto" }
        );
        assert!(!request.body_json()["tools"].as_array().unwrap().is_empty());
    }
    assert_eq!(
        sent[2 * limit - 1].body_json()["tools"],
        sent[2 * limit - 2].body_json()["tools"]
    );
    assert_eq!(
        sent[2 * limit - 1].body_json()["parallel_tool_calls"],
        sent[2 * limit - 2].body_json()["parallel_tool_calls"]
    );
    assert!(sent[2 * limit].body_contains_text("Later user input may resume work"));
    Ok(())
}

#[test]
fn mid_turn_compaction_failure_preserves_completed_message() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "mid_turn_compaction_failure_preserves_completed_message",
        mid_turn_compaction_failure_preserves_completed_message_impl,
    )
}

async fn mid_turn_compaction_failure_preserves_completed_message_impl() -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let requests = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("before-compaction"),
                responses::ev_assistant_message("completed-work", "The first result is ready."),
                responses::ev_function_call("continue-work", "unknown_test_tool", "{}"),
                responses::ev_completed_with_tokens("before-compaction", 90_001),
            ])),
            ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"detail": "permanent compaction failure"})),
        ],
    )
    .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
        config.compact_prompt = Some(codex_prompts::SUMMARIZATION_PROMPT.to_string());
        config.model_context_window = Some(100_000);
        config.model_auto_compact_token_limit = Some(90_000);
    });
    let test = builder.build(&server).await?;
    let completion = test
        .submit_turn_and_capture_completion("Produce a result, then continue working.")
        .await?;
    assert!(
        completion.error.is_some(),
        "compaction failure must remain visible"
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("The first result is ready.")
    );
    let requests = requests.requests();
    assert_eq!(
        requests.len(),
        2,
        "failure must not dispatch another generation"
    );
    assert!(
        requests[1]
            .function_call_output_text("continue-work")
            .is_none(),
        "unread tool results must remain outside the summarizer input"
    );
    test.codex.flush_rollout().await?;
    let rollout = fs::read_to_string(test.codex.rollout_path().expect("rollout path"))?;
    assert!(
        rollout
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|line| {
                line["type"] == "response_item"
                    && line["payload"]["type"] == "function_call_output"
                    && line["payload"]["call_id"] == "continue-work"
            }),
        "compaction failure must preserve the completed tool result in history"
    );
    Ok(())
}

#[test]
fn protocol_continuation_runs_required_work_without_new_input() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "protocol_continuation_runs_required_work_without_new_input",
        protocol_continuation_runs_required_work_without_new_input_impl,
    )
}

async fn protocol_continuation_runs_required_work_without_new_input_impl() -> Result<()> {
    crate::test_harness::require_network!();
    for response_kind in ["commentary", "reasoning", "empty"] {
        let server = responses::start_mock_server().await;
        let mut completed = responses::ev_completed("continuing");
        completed["response"]["end_turn"] = serde_json::json!(false);
        let mut initial = vec![responses::ev_response_created("continuing")];
        match response_kind {
            "commentary" => {
                let mut message = responses::ev_assistant_message(
                    "progress",
                    "I will now write the requested result.",
                );
                message["item"]["phase"] = serde_json::json!("commentary");
                initial.push(message);
            }
            "reasoning" => initial.push(responses::ev_reasoning_item(
                "reasoning",
                &["The requested file still needs to be written."],
                &[],
            )),
            _ => {}
        }
        initial.push(completed);
        let requests = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(initial),
                responses::sse(vec![
                    responses::ev_response_created("write-result"),
                    responses::ev_apply_patch_custom_tool_call(
                        "required-write",
                        "*** Begin Patch\n*** Add File: result.txt\n+requested result\n*** End Patch",
                    ),
                    responses::ev_completed("write-result"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("answer", "The result was written."),
                    responses::ev_completed("finished"),
                ]),
            ],
        ).await;
        let test = test_codex()
            .with_config(|config| {
                config.features.enable(Feature::Kd4Runtime).unwrap();
                config.features.disable(Feature::CodeModeHost).unwrap();
            })
            .build(&server)
            .await?;
        let completion = test
            .submit_turn_and_capture_completion("Write result.txt.")
            .await?;
        assert!(
            completion.error.is_none(),
            "{response_kind}: {completion:?}"
        );
        assert_eq!(
            fs::read_to_string(test.workspace_path("result.txt"))?,
            "requested result\n"
        );
        assert_eq!(
            completion.last_agent_message.as_deref(),
            Some("The result was written.")
        );
        assert_eq!(requests.requests().len(), 3, "{response_kind}");
        let timing = completion.timing.expect("completed turn timing");
        assert_eq!(
            timing.counters.residual_deterministic_generation_count,
            None
        );
    }
    Ok(())
}

#[test]
fn passing_focused_validation_keeps_the_second_change_executable() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "passing_focused_validation_keeps_the_second_change_executable",
        passing_focused_validation_keeps_the_second_change_executable_impl,
    )
}

async fn passing_focused_validation_keeps_the_second_change_executable_impl() -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let python = if cfg!(windows) { "python" } else { "python3" };
    let mut sequence = Vec::new();
    for part in ["first", "second"] {
        let patch = format!(
            "*** Begin Patch\n*** Add File: {part}.txt\n+{part} result\n*** Add File: test_{part}.py\n+import unittest\n+from pathlib import Path\n+class ResultTest(unittest.TestCase):\n+    def test_result(self):\n+        self.assertEqual(Path('{part}.txt').read_text(), '{part} result\\n')\n*** End Patch"
        );
        sequence.push(responses::sse(vec![
            responses::ev_response_created(&format!("patch-{part}")),
            responses::ev_apply_patch_custom_tool_call(&format!("patch-{part}"), &patch),
            responses::ev_completed(&format!("patch-{part}")),
        ]));
        sequence.push(responses::sse(vec![
            responses::ev_response_created(&format!("validate-{part}")),
            responses::ev_shell_command_call(
                &format!("validate-{part}"),
                &format!("{python} -m unittest test_{part} -q"),
            ),
            responses::ev_completed(&format!("validate-{part}")),
        ]));
    }
    sequence.push(responses::sse(vec![
        responses::ev_assistant_message("answer", "Both changes passed their focused tests."),
        responses::ev_completed("finished"),
    ]));
    let requests = responses::mount_sse_sequence(&server, sequence).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Kd4Runtime).unwrap();
            config.features.disable(Feature::CodeModeHost).unwrap();
            config.features.disable(Feature::UnifiedExec).unwrap();
        })
        .build(&server)
        .await?;
    let completion = test.submit_turn_and_capture_completion(
        "Implement the first result and validate it, then implement and validate the second result.",
    ).await?;
    assert!(completion.error.is_none(), "{completion:?}");
    for part in ["first", "second"] {
        assert_eq!(
            fs::read_to_string(test.workspace_path(format!("{part}.txt")))?,
            format!("{part} result\n")
        );
    }
    let requests = requests.requests();
    assert_eq!(requests.len(), 5);
    for (request_index, part) in [(2, "first"), (4, "second")] {
        let output = requests[request_index]
            .function_call_output_text(&format!("validate-{part}"))
            .expect("model receives the completed validation");
        assert!(output.contains("Ran 1 test"), "{output}");
        assert!(output.contains("OK"), "{output}");
        assert!(
            requests[request_index].body_json()["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()),
            "passing a focused test must not remove the next action's tools"
        );
    }
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("Both changes passed their focused tests.")
    );
    Ok(())
}

#[test]
fn repeated_failed_read_preserves_a_different_required_action() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "repeated_failed_read_preserves_a_different_required_action",
        repeated_failed_read_preserves_a_different_required_action_impl,
    )
}

async fn repeated_failed_read_preserves_a_different_required_action_impl() -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let mut sequence = Vec::new();
    for index in 1..=3 {
        sequence.push(responses::sse(vec![
            responses::ev_response_created(&format!("repeat-{index}")),
            responses::ev_function_call(&format!("repeat-{index}"), "read_tool_output",
                r#"{"artifact_id":"11111111-1111-4111-8111-111111111111","selectors":[{"kind":"lines","start":1,"end":1}]}"#),
            responses::ev_completed(&format!("repeat-{index}")),
        ]));
    }
    sequence.push(responses::sse(vec![
        responses::ev_response_created("different-action"),
        responses::ev_apply_patch_custom_tool_call("required-write",
            "*** Begin Patch\n*** Add File: recovered.txt\n+recovered through a different action\n*** End Patch"),
        responses::ev_completed("different-action"),
    ]));
    sequence.push(responses::sse(vec![
        responses::ev_assistant_message("answer", "Recovered and completed the required change."),
        responses::ev_completed("finished"),
    ]));
    let requests = responses::mount_sse_sequence(&server, sequence).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Kd4Runtime).unwrap();
            config.features.disable(Feature::CodeModeHost).unwrap();
        })
        .build(&server)
        .await?;
    let completion = test
        .submit_turn_and_capture_completion("Recover the evidence, then write the required result.")
        .await?;
    assert!(completion.error.is_none(), "{completion:?}");
    assert_eq!(
        fs::read_to_string(test.workspace_path("recovered.txt"))?,
        "recovered through a different action\n"
    );
    let requests = requests.requests();
    assert_eq!(requests.len(), 5);
    // Missing-artifact errors do not claim to be permanent, so they remain
    // retryable. Their repeated cycle still exercises convergence without
    // permission to disable a different required action.
    for (index, request) in requests.iter().enumerate().take(4).skip(1) {
        let failure = request
            .function_call_output_text(&format!("repeat-{index}"))
            .expect("repeated artifact lookup failure");
        assert!(
            failure.contains("artifact expired or does not belong to this thread"),
            "a valid artifact lookup should reach the repeated failure path: {failure}"
        );
    }
    assert!(
        requests[3]
            .body_json()
            .to_string()
            .contains("Other tools remain available for unfinished work and recovery.")
    );
    assert!(
        requests[3].body_json()["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("Recovered and completed the required change.")
    );
    Ok(())
}

fn complete_compaction_summary(state: &str) -> String {
    format!(
        "## Goal\nresume the pending turn\n\n## Current state\n{state}\n\n## Completed work\nseed turn completed\n\n## Unresolved work\npending input remains\n\n## Evidence\nmock compaction response\n\n## Next action\nsample the pending input"
    )
}

#[test]
fn compaction_that_remains_over_limit_is_not_a_retryable_stream_error() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "compaction_that_remains_over_limit_is_not_a_retryable_stream_error",
        compaction_that_remains_over_limit_is_not_a_retryable_stream_error_impl,
    )
}

async fn compaction_that_remains_over_limit_is_not_a_retryable_stream_error_impl() -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![responses::sse(vec![
            responses::ev_response_created("compaction"),
            responses::ev_assistant_message(
                "summary",
                &complete_compaction_summary("work remains"),
            ),
            responses::ev_completed("compaction"),
        ])],
    )
    .await;
    let home = tempfile::tempdir()?;
    let provider = non_openai_model_provider(&server);
    let (session, turn_context, _) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            move |config| {
                config.model_provider = provider;
                config.model_provider.stream_max_retries = Some(0);
                config.model_auto_compact_token_limit = Some(1);
                config.model_auto_compact_token_limit_scope =
                    codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            },
        )
        .await;
    let step = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let mut client = session.services.model_client.new_session();
    let error = run_auto_compact(
        &session,
        step,
        None,
        &mut client,
        None,
        None,
        CompactionReason::ContextLimit,
        CompactionPhase::MidTurn,
        ConsumedOutputRetirement::Eligible {
            projected_pressure: None,
        },
        &CancellationToken::new(),
    )
    .await
    .expect_err("the preserved context cannot fit in a single token");
    assert!(
        error
            .to_string()
            .contains("Compaction did not bring the context below")
    );
    assert!(!error.is_retryable());
    assert_eq!(error.to_codex_protocol_error(), CodexErrorInfo::Other);
    assert_eq!(requests.requests().len(), 1);
    Ok(())
}

#[test]
fn projected_pressure_retirement_declines_when_the_projected_prompt_still_overflows() -> Result<()>
{
    run_turn_multi_thread_test_with_stack(
        "projected_pressure_retirement_declines_when_the_projected_prompt_still_overflows",
        projected_pressure_retirement_declines_when_the_projected_prompt_still_overflows_impl,
    )
}

/// Seeds consumed, pin-eligible tool results that context-pressure retirement
/// can select, mirroring the fixture in `context_manager::history_tests`.
async fn seed_retirable_consumed_outputs(session: &Session, count: u32) {
    let mut state = session.state.lock().await;
    let history = &mut state.history;
    let evidence = |index: u32| format!("result-{index} {}", "evidence ".repeat(500));
    for index in 0..count {
        let call_id = format!("read-{index}");
        assert!(history.apply_tool_history_mutation(
            &crate::tool_history::ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
                call_id: call_id.clone(),
            },
        ));
        assert!(
            history.apply_tool_history_mutation(
                &crate::tool_history::ToolHistoryMutation::RegisterCandidate {
                    candidate: crate::tool_history::ToolHistoryCandidate {
                        call_id: call_id.clone(),
                        tool_identity: "functions.exec".to_string(),
                        semantic_class: "tool_output".to_string(),
                        successful: true,
                        source_dependencies: std::collections::BTreeSet::new(),
                        source_dependencies_current: true,
                        artifact_id: format!("artifact-{call_id}"),
                        artifact_bytes: 96_000,
                        artifact_sha256: crate::tool_history::sha256(b"canonical artifact"),
                        original_output_sha256: crate::tool_history::sha256(
                            evidence(index).as_bytes(),
                        ),
                        original_tokens: 24_000,
                        preserved_non_text_tokens: Some(0),
                        bounded_model_output: evidence(index),
                        complete: true,
                        projection_eligible: true,
                        proof_identity: None,
                        supersession_identity: None,
                        consumed_by_generation: None,
                        derived: crate::tool_history::ToolHistoryCandidateDerived::default(),
                    },
                },
            )
        );
        history.record_items(
            [
                &ResponseItem::FunctionCall {
                    id: None,
                    name: "functions.exec".to_string(),
                    namespace: None,
                    arguments: "{}".to_string(),
                    call_id: call_id.clone(),
                    internal_chat_message_metadata_passthrough: None,
                },
                &ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: call_id.clone(),
                    output: codex_protocol::models::FunctionCallOutputPayload::from_text(evidence(
                        index,
                    )),
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            TruncationPolicy::Tokens(10_000),
        );
        // One generation reads each result, as in a turn of single reads.
        let sent = history.clone().prepare_for_prompt(&[]);
        history.mark_tool_history_consumed_with_delta(
            sent.items(),
            crate::tool_history::ModelGenerationId {
                turn_id: "turn".to_string(),
                ordinal: index,
            },
        );
    }
}

/// The pre-turn trigger fires on the next prompt's projection, which includes
/// pending input that live usage does not yet contain. Retirement must judge
/// itself against that same projection: judged on live usage alone it would
/// report that an overflowing prompt fits, and the planning loop treats a
/// second overflow as a hard failure rather than retrying.
async fn projected_pressure_retirement_declines_when_the_projected_prompt_still_overflows_impl()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let (session, turn_context, _) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |config| {
                config.model_auto_compact_token_limit = Some(100_000);
                config.model_auto_compact_token_limit_scope =
                    codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            },
        )
        .await;
    seed_retirable_consumed_outputs(&session, 8).await;

    // Live usage sits under the limit, so retiring these results would clear it
    // and report that no summary is needed.
    let live_tokens = session.get_total_token_usage().await;
    assert!(
        live_tokens < 100_000,
        "fixture must leave live usage under the limit, got {live_tokens}"
    );

    // The projected prompt is far beyond anything retirement can recover, so it
    // must decline and leave the summary to run. Judged on live usage this same
    // call would retire and return true.
    let projected = ProjectedPromptPressure {
        total_tokens: 10_000_000,
        auto_compact_scope_tokens: 10_000_000,
    };
    assert!(
        !retire_consumed_tool_outputs_for_context_pressure(
            &session,
            &turn_context,
            CONTEXT_PRESSURE_KEPT_GENERATIONS,
            Some(projected),
        )
        .await,
        "retirement must not claim an overflowing projected prompt fits"
    );
    assert!(
        !crate::tool_history::ToolHistoryState::has_phase_checkpoint(
            session.state.lock().await.history.raw_items()
        ),
        "a declined retirement must not retire any evidence"
    );

    // The same pressure within reach is retired rather than summarized. Each
    // retirement saves only the inline output net of its pin, so overflow the
    // limit by half of what the seeded evidence can actually recover.
    let limit = turn_context
        .model_info
        .auto_compact_token_limit()
        .expect("fixture configures an automatic-compaction limit");
    let (_, retirable) = session
        .state
        .lock()
        .await
        .history
        .pressure_retirement_selection(CONTEXT_PRESSURE_KEPT_GENERATIONS, u64::MAX);
    let retirable = i64::try_from(retirable)?;
    assert!(retirable > 0, "fixture must seed retirable evidence");
    let reachable = ProjectedPromptPressure {
        total_tokens: limit + retirable / 2,
        auto_compact_scope_tokens: limit + retirable / 2,
    };
    assert!(
        retire_consumed_tool_outputs_for_context_pressure(
            &session,
            &turn_context,
            CONTEXT_PRESSURE_KEPT_GENERATIONS,
            Some(reachable),
        )
        .await,
        "retirement must clear a projected prompt its pins can bring under the limit"
    );
    assert!(
        crate::tool_history::ToolHistoryState::has_phase_checkpoint(
            session.state.lock().await.history.raw_items()
        ),
        "the accepted retirement records a checkpoint"
    );
    Ok(())
}

#[test]
fn failed_local_compaction_falls_back_to_exact_retirement() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "failed_local_compaction_falls_back_to_exact_retirement",
        failed_local_compaction_falls_back_to_exact_retirement_impl,
    )
}

/// Audit 146: a provider without remote compaction (or a custom compact
/// prompt) has no remote summary to fall back from. A rejected local summary
/// gets the exact retirement a rejected remote summary already gets, and the
/// turn continues only when that retirement clears the measured pressure.
async fn failed_local_compaction_falls_back_to_exact_retirement_impl() -> Result<()> {
    crate::test_harness::require_network!();
    for reachable in [true, false] {
        let server = responses::start_mock_server().await;
        // Neither the summary nor its one corrective attempt is a checkpoint.
        let rejected = || {
            responses::sse(vec![
                responses::ev_response_created("compaction"),
                responses::ev_assistant_message("summary", "not a checkpoint"),
                responses::ev_completed("compaction"),
            ])
        };
        let requests =
            responses::mount_sse_sequence(&server, vec![rejected(), rejected()]).await;
        let home = tempfile::tempdir()?;
        let provider = non_openai_model_provider(&server);
        let (session, turn_context, _) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("test key"),
                Vec::new(),
                home.path(),
                move |config| {
                    config.model_provider = provider;
                    config.model_provider.stream_max_retries = Some(0);
                    config.model_auto_compact_token_limit = Some(100_000);
                    config.model_auto_compact_token_limit_scope =
                        codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
                },
            )
            .await;
        // Four single-read generations: routine retirement keeps the newest
        // three inline, retirement after a failed summary keeps only one.
        seed_retirable_consumed_outputs(&session, 4).await;
        let limit = turn_context
            .model_info
            .auto_compact_token_limit()
            .expect("fixture configures an automatic-compaction limit");
        let (routine, after_failure) = {
            let state = session.state.lock().await;
            (
                state
                    .history
                    .pressure_retirement_selection(CONTEXT_PRESSURE_KEPT_GENERATIONS, u64::MAX)
                    .1,
                state
                    .history
                    .pressure_retirement_selection(FAILED_COMPACTION_KEPT_GENERATIONS, u64::MAX)
                    .1,
            )
        };
        let (routine, after_failure) = (i64::try_from(routine)?, i64::try_from(after_failure)?);
        assert!(
            routine > 0 && after_failure > routine,
            "fixture must leave more to retire after a failed summary"
        );
        // Routine retirement cannot clear either overflow, so the summary runs.
        let overflow = if reachable {
            (routine + after_failure) / 2
        } else {
            2 * after_failure
        };
        let pressure = ProjectedPromptPressure {
            total_tokens: limit + overflow,
            auto_compact_scope_tokens: limit + overflow,
        };
        let step = session
            .capture_step_context(Arc::clone(&turn_context))
            .await
            .unwrap();
        let mut client = session.services.model_client.new_session();
        let result = run_auto_compact(
            &session,
            step,
            None,
            &mut client,
            None,
            None,
            CompactionReason::ContextLimit,
            CompactionPhase::PreTurn,
            ConsumedOutputRetirement::Eligible {
                projected_pressure: Some(pressure),
            },
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            requests.requests().len(),
            2,
            "the summary and its corrective attempt were both requested"
        );
        let retired = crate::tool_history::ToolHistoryState::has_phase_checkpoint(
            session.state.lock().await.history.raw_items(),
        );
        if reachable {
            assert_eq!(
                result.expect("exact retirement continues the turn"),
                ContextPressureRelief::Compaction
            );
            assert!(retired, "the fallback records its retirement checkpoint");
        } else {
            let error = result.expect_err("retirement cannot clear this pressure");
            assert!(
                error
                    .to_string()
                    .contains("default compaction requires structured checkpoint sections"),
                "the summary failure is reported unchanged: {error}"
            );
            assert!(!retired, "a declined fallback must not retire evidence");
        }
    }
    Ok(())
}

#[test]
fn recovered_compaction_failure_does_not_tell_the_user_to_start_over() {
    let overflow = recovered_compaction_failure(&CodexErr::ContextWindowExceeded);
    assert!(!overflow.contains("Start a new thread"), "{overflow}");
    assert!(overflow.contains("context window"), "{overflow}");
    // Negative control: other failures keep their own text, without the
    // doubled period the caller's sentence would otherwise add.
    assert_eq!(
        recovered_compaction_failure(&CodexErr::Fatal("summary rejected.".into())),
        "Fatal error: summary rejected"
    );
}

#[test]
fn proactive_retirement_waits_until_resending_outputs_pays_for_the_cache_miss() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "proactive_retirement_waits_until_resending_outputs_pays_for_the_cache_miss",
        proactive_retirement_waits_until_resending_outputs_pays_for_the_cache_miss_impl,
    )
}

/// Long sessions sit just above the steady-state ceiling. Retiring there
/// whenever 20k tokens qualified made each next request re-read the whole
/// remaining history uncached to save a small batch, every few requests.
/// Re-sending a cached token costs a tenth of reading an uncached one, so
/// retirement must wait until the requests that re-sent the batch have paid
/// for that re-read, and then retire once.
async fn proactive_retirement_waits_until_resending_outputs_pays_for_the_cache_miss_impl()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let (session, turn_context, _) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            CodexAuth::from_api_key("test key"),
            Vec::new(),
            home.path(),
            |config| {
                config.model_auto_compact_token_limit = Some(100_000);
                config.model_auto_compact_token_limit_scope =
                    codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            },
        )
        .await;
    seed_retirable_consumed_outputs(&session, 75).await;
    // The last completed request reported a prompt between the steady-state
    // ceiling and the limit, where the long sessions sat.
    let reported = codex_protocol::protocol::TokenUsage {
        input_tokens: 90_000,
        total_tokens: 90_000,
        ..Default::default()
    };
    session
        .state
        .lock()
        .await
        .set_token_info(Some(codex_protocol::protocol::TokenUsageInfo {
            estimated_context_tokens: None,
            total_token_usage: reported.clone(),
            last_token_usage: reported,
            model_context_window: None,
        }));

    let limit = turn_context
        .model_info
        .auto_compact_token_limit()
        .expect("fixture configures an automatic-compaction limit");
    let target = limit * 2 / 3;
    let scope =
        crate::session::context_window::context_window_token_status(&session, &turn_context)
            .await
            .auto_compact_scope_tokens;
    assert!(
        scope > target && scope < limit,
        "fixture must sit between the steady-state ceiling and the limit, got {scope} of {limit}"
    );
    let (selected, saved) = session
        .state
        .lock()
        .await
        .history
        .pressure_retirement_selection(
            CONTEXT_PRESSURE_KEPT_GENERATIONS,
            u64::try_from(scope - target)?,
        );
    assert!(
        !selected.is_empty() && saved >= 20_000,
        "the former rule retired any batch of at least 20k tokens, got {saved}"
    );
    let history_tokens = session
        .state
        .lock()
        .await
        .history
        .estimate_prepared_token_count_with_base_instructions(
            &turn_context.model_info.input_modalities,
            &BaseInstructions {
                text: String::new(),
            },
        )
        .expect("history estimate");
    // Each request that re-sends the batch pays saved / 10; retiring makes the
    // next request re-read the remaining history at the 9 / 10 difference.
    let rewritten = u64::try_from(history_tokens)? - saved;
    let break_even = (9 * rewritten).div_ceil(saved);
    assert!(
        break_even > 1,
        "fixture must make an immediate retirement uneconomic"
    );

    for request in 1..break_even {
        assert!(
            !retire_consumed_tool_outputs_before_limit(&session, &turn_context).await,
            "request {request} of {break_even} has not yet paid for the cache miss"
        );
    }
    assert!(
        !crate::tool_history::ToolHistoryState::has_phase_checkpoint(
            session.state.lock().await.history.raw_items()
        ),
        "a deferred retirement must leave history unchanged"
    );
    assert!(
        retire_consumed_tool_outputs_before_limit(&session, &turn_context).await,
        "the request whose charge covers the cache miss retires the batch"
    );
    let state = session.state.lock().await;
    let checkpoint = state
        .history
        .raw_items()
        .iter()
        .rev()
        .find_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => {
                content.iter().find_map(|part| match part {
                    codex_protocol::models::ContentItem::InputText { text }
                        if crate::tool_history::is_pressure_retirement_checkpoint_text(text) =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("the retirement records a checkpoint");
    assert!(
        checkpoint.contains(r#""trigger":"proactive""#),
        "the checkpoint labels its trigger: {checkpoint}"
    );
    assert_eq!(
        state.proactive_retirement_carried_tokens, 0,
        "carrying restarts after the rewrite"
    );
    Ok(())
}

/// A session rewrote an existing test's expectation without mentioning it in
/// its summary. Completion names edited existing tests by common convention.
#[test]
fn completion_advisory_names_changed_existing_tests() {
    for path in [
        "scripts/benchmark_scripts/test_rollout_audit_cache.py",
        "tests/integration.py",
        "core/src/session/turn_tests.rs",
        "core/src/tests.rs",
        "web/src/__tests__/app.tsx",
        "web/src/app.test.ts",
        "pkg/server_test.go",
        "spec/models/user_spec.rb",
        "src/Foo.Tests/Parser.cs",
        "conftest.py",
    ] {
        assert!(conventional_test_path(path), "{path} is a test file");
    }
    for path in [
        "core/src/session/turn.rs",
        "docs/testing.md",
        "scripts/latest.py",
        "attestation/verify.py",
    ] {
        assert!(!conventional_test_path(path), "{path} is not a test file");
    }
    assert_eq!(changed_tests_advisory(&[]), None);
    assert_eq!(
        changed_tests_advisory(&["tests/test_cli.py".to_string(), "a_test.go".to_string()])
            .as_deref(),
        Some(
            "This turn modified or deleted existing test files; review any changed expectations or execution policy: tests/test_cli.py, a_test.go."
        )
    );
}

/// Recorded sessions: one rewrote a fixture inside an inline `#[cfg(test)]`
/// module of `callable_collector.rs` without an advisory, while another only
/// appended a regression test to `profile_matrix_tests.rs` and was flagged.
#[test]
fn completion_advisory_detects_oracle_activation_and_occurrence_changes() {
    let python = "import unittest\nEXPECTED = 1\nclass TestContract(unittest.TestCase):\n    EXPECTED = 1\n    def test_contract(self):\n        self.assertEqual(2, EXPECTED)\n";
    let rust = "#[cfg(test)]\nmod tests { #[test] fn contract() { assert!(true); } }\n";
    let duplicate = "#[cfg(unix)]\nmod tests { #[test] fn contract() {} }\n#[cfg(windows)]\nmod tests { #[test] fn contract() {} }\n";
    for (path, before, after) in [
        (
            "tests/test_contract.py",
            python,
            python.replacen("EXPECTED = 1", "EXPECTED = 2", 1),
        ),
        (
            "tests/test_contract.py",
            python,
            python.replace("    EXPECTED = 1", "    EXPECTED = 2"),
        ),
        (
            "tests/test_contract.py",
            python,
            python.replace(
                "class TestContract",
                "@unittest.skip('temporary')\nclass TestContract",
            ),
        ),
        ("src/lib.rs", rust, rust.replace("cfg(test)", "cfg(any())")),
        (
            "src/lib.rs",
            duplicate,
            "#[cfg(unix)]\nmod tests { #[test] fn contract() {} }\n".to_string(),
        ),
        // Same header and leaf text still describe two distinct occurrences.
        (
            "src/lib.rs",
            "#[cfg(test)] mod tests { #[test] fn a() {} }\n#[cfg(test)] mod tests { #[test] fn a() {} }",
            "#[cfg(test)] mod tests { #[test] fn a() {} }".to_string(),
        ),
    ] {
        let result =
            changed_existing_tests(vec![(path.to_string(), before.to_string(), Some(after))]);
        assert_eq!(result.changed, [path], "{before}");
        assert!(result.incomplete.is_empty());
    }
    let two = "def test_first():\n    assert True\n\ndef test_second():\n    assert True\n";
    let moved = "def test_second():\n    assert True\n\ndef test_first():\n    assert True\n";
    let result = changed_existing_tests(vec![(
        "test_move.py".to_string(),
        two.to_string(),
        Some(moved.to_string()),
    )]);
    assert!(
        result.changed.is_empty(),
        "moving tests within the same policy is not a rewrite"
    );
    assert!(result.incomplete.is_empty());
}

#[test]
fn completion_advisory_keeps_incomplete_analysis_separate_from_test_edits() {
    let result = changed_existing_tests(vec![(
        "src/broken.rs".to_string(),
        "fn broken(".to_string(),
        Some("fn broken(".to_string()),
    )]);
    assert!(result.changed.is_empty());
    assert_eq!(result.incomplete, ["src/broken.rs"]);
    let advisory = incomplete_test_evidence_advisory(&result.incomplete).unwrap();
    assert!(advisory.contains("could not be fully assessed"));
    assert!(advisory.contains("uncertain before/after attribution"));
    assert_eq!(incomplete_test_evidence_advisory(&[]), None);
    assert_eq!(
        crate::tools::handlers::existing_tests_changed(
            "test_deadline.py",
            "def test_a(): pass",
            Some("def test_a(): pass"),
            true,
            std::time::Instant::now(),
        ),
        None,
        "deadline exhaustion cannot certify additions-only behavior",
    );
}

#[test]
fn completion_advisory_reports_edited_existing_tests_not_added_ones() {
    let inline = "pub fn run() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    fn fixture() -> u8 {\n        super::run()\n    }\n\n    #[test]\n    fn runs() {\n        assert_eq!(fixture(), 1);\n    }\n}\n";
    let test_file = "#[test]\nfn first() {\n    assert!(true);\n}\n";
    let go_test = "func TestA(t *testing.T) {\n\tcheck(t)\n}\n";
    let changed = |path: &str, before: &str, after: Option<&str>| {
        changed_existing_tests(vec![(
            path.to_string(),
            before.to_string(),
            after.map(str::to_string),
        )])
        .changed
    };
    let edited_fixture = inline.replace("super::run()\n", "super::run() + 0\n");
    assert_eq!(
        changed("src/callable_collector.rs", inline, Some(&edited_fixture)),
        ["src/callable_collector.rs"]
    );
    let ignored = inline.replace("    #[test]\n", "    #[test]\n    #[ignore]\n");
    assert_eq!(
        changed("src/callable_collector.rs", inline, Some(&ignored)),
        ["src/callable_collector.rs"]
    );
    let production_only = inline.replace("    1\n", "    2 - 1\n");
    assert!(changed("src/callable_collector.rs", inline, Some(&production_only)).is_empty());
    let added_inline = inline.replace(
        "        assert_eq!(fixture(), 1);\n    }\n",
        "        assert_eq!(fixture(), 1);\n    }\n\n    #[test]\n    fn runs_twice() {\n        assert_eq!(fixture() + fixture(), 2);\n    }\n",
    );
    assert!(changed("src/callable_collector.rs", inline, Some(&added_inline)).is_empty());

    let appended = format!("{test_file}\n#[test]\nfn second() {{\n    assert!(true);\n}}\n");
    assert!(
        changed(
            "src/cli/profile_matrix_tests.rs",
            test_file,
            Some(&appended)
        )
        .is_empty()
    );
    let weakened = test_file.replace("    assert!(true);\n", "");
    assert_eq!(
        changed(
            "src/cli/profile_matrix_tests.rs",
            test_file,
            Some(&weakened)
        ),
        ["src/cli/profile_matrix_tests.rs"]
    );
    assert_eq!(
        changed("src/cli/profile_matrix_tests.rs", test_file, None),
        ["src/cli/profile_matrix_tests.rs"]
    );

    // Without an outline, conventional test files report rewritten lines only.
    let go_appended = format!("{go_test}\nfunc TestB(t *testing.T) {{}}\n");
    assert!(changed("pkg/server_test.go", go_test, Some(&go_appended)).is_empty());
    let go_edited = go_test.replace("\tcheck(t)\n", "");
    assert_eq!(
        changed("pkg/server_test.go", go_test, Some(&go_edited)),
        ["pkg/server_test.go"]
    );
}

fn write_one_shot_stop_hook(home: &Path) -> Result<()> {
    let script_path = home.join("phase_68_stop_hook.py");
    let counter_path = home.join("phase_68_stop_hook.count");
    let counter_path = serde_json::to_string(&counter_path.to_string_lossy())?;
    let script = format!(
        r#"import json
from pathlib import Path
import sys

json.load(sys.stdin)
counter_path = Path({counter_path})
if not counter_path.exists():
    counter_path.write_text("1", encoding="utf-8")
    print(json.dumps({{"decision": "block", "reason": "continue after evidence warning"}}))
else:
    print(json.dumps({{"systemMessage": "stop hook continuation complete"}}))
"#,
    );
    let command = format!("python3 \"{}\"", script_path.display());
    let command_windows = format!("python \"{}\"", script_path.display());
    let hooks = serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "commandWindows": command_windows,
                }]
            }]
        }
    });
    fs::write(script_path, script)?;
    fs::write(home.join("hooks.json"), hooks.to_string())?;
    Ok(())
}

#[test]
fn registered_turn_stop_panic_publishes_one_failed_terminal() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "registered_turn_stop_panic_publishes_one_failed_terminal",
        || terminal_publication_survives_registered_contributor_panic(false),
    )
}

#[test]
fn registered_thread_idle_panic_preserves_one_successful_terminal() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "registered_thread_idle_panic_preserves_one_successful_terminal",
        || terminal_publication_survives_registered_contributor_panic(true),
    )
}

async fn terminal_publication_survives_registered_contributor_panic(
    panic_after_publication: bool,
) -> Result<()> {
    use codex_app_server_protocol::ThreadHistoryBuilder;
    use codex_app_server_protocol::TurnStatus;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::RolloutItem;

    struct HealthyTurnObserver {
        starts: Arc<std::sync::atomic::AtomicUsize>,
        stops: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl codex_extension_api::TurnLifecycleContributor for HealthyTurnObserver {
        fn on_turn_start<'a>(
            &'a self,
            _input: codex_extension_api::TurnStartInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
            })
        }
        fn on_turn_stop<'a>(
            &'a self,
            _input: codex_extension_api::TurnStopInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
            })
        }
    }

    struct StopPanic {
        reached: async_channel::Sender<()>,
    }
    impl codex_extension_api::TurnLifecycleContributor for StopPanic {
        fn on_turn_stop<'a>(
            &'a self,
            _input: codex_extension_api::TurnStopInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                self.reached.send(()).await.expect("panic observer open");
                panic!("installed turn-stop contributor failed before terminal publication");
            })
        }
    }

    struct IdlePanic {
        reached: async_channel::Sender<()>,
        fired: std::sync::atomic::AtomicBool,
    }
    impl<C: Sync> codex_extension_api::ThreadLifecycleContributor<C> for IdlePanic {
        fn on_thread_idle<'a>(
            &'a self,
            _input: codex_extension_api::ThreadIdleInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                if !self.fired.swap(true, Ordering::SeqCst) {
                    self.reached.send(()).await.expect("panic observer open");
                    panic!("installed thread-idle contributor failed after terminal publication");
                }
            })
        }
    }

    let server = responses::start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![responses::sse(vec![
            responses::ev_response_created("terminal-publication-response"),
            responses::ev_assistant_message(
                "terminal-publication-message",
                "The completed answer is 42.",
            ),
            responses::ev_completed("terminal-publication-response"),
        ])],
    )
    .await;
    let (reached_tx, reached_rx) = async_channel::bounded(1);
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    if panic_after_publication {
        extensions.thread_lifecycle_contributor(Arc::new(IdlePanic {
            reached: reached_tx,
            fired: std::sync::atomic::AtomicBool::new(false),
        }));
    } else {
        extensions.turn_lifecycle_contributor(Arc::new(StopPanic {
            reached: reached_tx,
        }));
    }
    let healthy_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let healthy_stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Register after the panicking stop contributor to prove later observers still run.
    extensions.turn_lifecycle_contributor(Arc::new(HealthyTurnObserver {
        starts: Arc::clone(&healthy_starts),
        stops: Arc::clone(&healthy_stops),
    }));
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config.model_provider = provider;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        });
    let test = builder.build(&server).await?;
    let rollout_path = test.codex.rollout_path().expect("physical rollout enabled");
    let turn_id = test
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Return the completed answer.".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let mut live_events = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = test.codex.next_event().await.expect("normal turn event");
            let terminal = matches!(&event.msg,
                EventMsg::TurnComplete(event) if event.turn_id == turn_id)
                || matches!(&event.msg,
                    EventMsg::TurnAborted(event) if event.turn_id.as_deref() == Some(turn_id.as_str()));
            live_events.push(event.msg);
            if terminal {
                break;
            }
        }
        // This observes the installed callback, not a production completion flag.
        reached_rx.recv().await.expect("installed panic callback ran");
    })
    .await
    .expect("normal turn must reach the installed panic and a terminal event");

    // Shutdown waits for the real terminal task owner and flushes physical history.
    // Continue consuming through its acknowledgment so a second terminal cannot hide
    // after the first event or behind a short quiet-period timeout.
    test.codex.submit(Op::Shutdown).await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = test.codex.next_event().await.expect("shutdown event");
            let shutdown = matches!(event.msg, EventMsg::ShutdownComplete);
            live_events.push(event.msg);
            if shutdown {
                break;
            }
        }
    })
    .await
    .expect("terminal finalizer and physical history must finish before shutdown");
    assert_eq!(request_log.requests().len(), 1, "one real model turn");
    assert_eq!(
        healthy_starts.load(Ordering::SeqCst),
        1,
        "normal registered start callback"
    );
    assert_eq!(
        healthy_stops.load(Ordering::SeqCst),
        1,
        "a healthy later stop observer runs exactly once despite another observer panic"
    );

    let initial =
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(&rollout_path).await?;
    let rollout = initial.get_rollout_items();
    let persisted_events = rollout
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(event) => Some(event.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected_error = "The turn finalizer failed before ordinary terminal cleanup completed.";
    let expected_status = if panic_after_publication {
        TurnStatus::Completed
    } else {
        TurnStatus::Failed
    };
    let mut terminal_payloads = Vec::new();
    for (boundary, events) in [
        ("live event stream", live_events.as_slice()),
        ("physical rollout", persisted_events.as_slice()),
    ] {
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event,
                EventMsg::TurnStarted(event) if event.turn_id == turn_id))
                .count(),
            1,
            "{boundary}: normal RegularTask starts exactly once"
        );
        let terminals = events
            .iter()
            .filter(|event| matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)))
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1, "{boundary}: one terminal per real turn");
        let EventMsg::TurnComplete(completion) = terminals[0] else {
            panic!("{boundary}: contributor panic must not turn completed work into interruption");
        };
        assert_eq!(completion.turn_id, turn_id);
        assert!(completion.surfaced_result.is_none());
        if panic_after_publication {
            assert_eq!(
                completion.last_agent_message.as_deref(),
                Some("The completed answer is 42.")
            );
            assert!(
                completion.error.is_none(),
                "{boundary}: published success must survive idle panic"
            );
        } else {
            let error = completion
                .error
                .as_ref()
                .expect("pre-publication panic needs a failure");
            assert_eq!(error.message, expected_error);
            assert_eq!(
                error.codex_error_info,
                Some(CodexErrorInfo::InternalServerError)
            );
            assert_eq!(
                completion.last_agent_message.as_deref(),
                Some(expected_error)
            );
        }
        terminal_payloads.push(serde_json::to_value(completion)?);
        let mut consumer = ThreadHistoryBuilder::new();
        for event in events {
            consumer.handle_event(event);
        }
        assert!(
            consumer.in_progress_turn_snapshot().is_none(),
            "{boundary}: consumer must be terminal"
        );
        let turns = consumer.finish();
        assert_eq!(
            turns.len(),
            1,
            "{boundary}: consumer must not fabricate a second turn"
        );
        assert_eq!(turns[0].id, turn_id);
        assert_eq!(turns[0].status, expected_status);
        assert_eq!(
            turns[0].error.as_ref().map(|error| error.message.as_str()),
            (!panic_after_publication).then_some(expected_error)
        );
    }
    assert_eq!(
        terminal_payloads[0], terminal_payloads[1],
        "physical history preserves the delivered terminal outcome"
    );
    let mut replay = ThreadHistoryBuilder::new();
    for item in rollout {
        replay.handle_rollout_item(item);
    }
    let turns = replay.finish();
    assert_eq!(turns.len(), 1, "normal physical replay contains one turn");
    assert_eq!(turns[0].id, turn_id);
    assert_eq!(turns[0].status, expected_status);
    assert_eq!(
        turns[0].error.as_ref().map(|error| error.message.as_str()),
        (!panic_after_publication).then_some(expected_error)
    );
    Ok(())
}

#[test]
fn registered_tool_completion_survives_worker_abort_before_history_commit() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "registered_tool_completion_survives_worker_abort_before_history_commit",
        registered_tool_completion_survives_worker_abort_before_history_commit_impl,
    )
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "Hold the contested owner to assert cancellation, admission, or cleanup behavior under contention"
)]
async fn registered_tool_completion_survives_worker_abort_before_history_commit_impl() -> Result<()>
{
    use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
    use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
    use codex_protocol::dynamic_tools::DynamicToolResponse;
    use codex_protocol::dynamic_tools::DynamicToolSpec;
    use codex_protocol::protocol::RolloutItem;
    use codex_utils_absolute_path::AbsolutePathBuf;

    let server = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("completed-tool-before-abort"),
                responses::ev_function_call("owned-result", "completion_probe", "{}"),
                responses::ev_completed("completed-tool-before-abort"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("after-worker-abort"),
                responses::ev_assistant_message(
                    "after-worker-abort-message",
                    "retained result received",
                ),
                responses::ev_completed("after-worker-abort"),
            ]),
        ],
    )
    .await;
    let home = tempfile::tempdir()?;
    let mut config = crate::config::test_config().await;
    config.codex_home = AbsolutePathBuf::from_absolute_path(home.path())?;
    config.cwd = config.codex_home.clone();
    config.model_provider = non_openai_model_provider(&server);
    config.model_provider.request_max_retries = Some(0);
    config.model_provider.stream_max_retries = Some(0);
    let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        home.path().to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let mut options = manager.start_thread_options(config);
    options.dynamic_tools = vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
        name: "completion_probe".to_string(),
        description: "Return the externally supplied result".to_string(),
        input_schema: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        defer_loading: false,
    })];
    let started = manager.start_thread_with_options(options).await?;
    let thread = started.thread;
    let session = Arc::clone(&thread.codex.session);
    let first_turn_id = thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Run the completion probe".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match thread.next_event().await.expect("tool-start event").msg {
                EventMsg::ItemStarted(event) if matches!(&event.item,
                    TurnItem::DynamicToolCall(item) if item.id == "owned-result" && item.tool == "completion_probe") => break,
                EventMsg::Error(error) => panic!("registered tool failed before response: {}", error.message),
                _ => {}
            }
        }
    }).await.expect("normal registered dynamic tool must start");
    let (turn, worker_abort, worker_done) = {
        let active = session.active_turn.lock().await;
        let task = active
            .as_ref()
            .and_then(|active| active.task.as_ref())
            .expect("actual running turn task");
        (
            Arc::clone(&task.turn_context),
            task.worker_abort_handle.clone(),
            Arc::clone(&task.worker_done),
        )
    };
    assert_eq!(turn.sub_id, first_turn_id);
    let post_context = "post-tool evidence belongs after the completed result";
    turn.queue_post_tool_contexts(
        "owned-result",
        vec![ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: post_context.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
    )
    .await;
    // Hold the real extraction mutex, rather than replacing the drain or its result.
    // A developer context also selects the real blocking identity-preparation path.
    let contexts = Arc::clone(&turn.pending_post_tool_contexts);
    let contexts_guard = contexts.lock().await;
    assert_eq!(
        session
            .durable_history_commits_in_flight
            .load(Ordering::Acquire),
        0,
        "prior history commits must finish before the external tool response",
    );
    let exact_output = "completed external result: 7 * 6 = 42";
    thread
        .submit(Op::DynamicToolResponse {
            turn_id: turn.sub_id.clone(),
            id: "owned-result".to_string(),
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: exact_output.to_string(),
                }],
                success: true,
            },
        })
        .await?;
    let completed_item = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let EventMsg::ItemCompleted(event) = thread
                .next_event()
                .await
                .expect("tool completion event")
                .msg
                && let TurnItem::DynamicToolCall(item) = event.item
                && item.id == "owned-result"
            {
                break item;
            }
        }
    })
    .await
    .expect("normal registered tool must publish its actual completed result");
    assert_eq!(completed_item.tool, "completion_probe");
    assert_eq!(
        completed_item.status,
        codex_protocol::items::DynamicToolCallStatus::Completed
    );
    assert_eq!(completed_item.success, Some(true));
    assert_eq!(
        completed_item.content_items,
        Some(vec![DynamicToolCallOutputContentItem::InputText {
            text: exact_output.to_string(),
        }])
    );
    // Synchronize with the real commit owner without finalizing timing state.
    // The context mutex remains held, so this owner cannot commit until released.
    tokio::time::timeout(Duration::from_secs(15), async {
        while session
            .durable_history_commits_in_flight
            .load(Ordering::Acquire)
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real completed result must reach its owner before context extraction");
    assert_eq!(
        requests.requests().len(),
        1,
        "blocked commit must not permit another sample"
    );
    assert!(!session.clone_history().await.raw_items().iter().any(|item|
        matches!(item, ResponseItem::FunctionCallOutput { call_id, .. } if call_id == "owned-result")),
        "the paused window is before this tool output enters live history");

    // Exercise the exact abort primitive used when terminal cleanup exceeds its grace.
    worker_abort.abort();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !worker_done.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the original sampling worker must actually be dropped");
    drop(contexts_guard);
    tokio::time::timeout(
        Duration::from_secs(15),
        session.flush_rollout_after_ordered_commits(&turn),
    )
    .await
    .expect("accepted completion owner must finish after caller cancellation")?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match thread
                .next_event()
                .await
                .expect("aborted-turn terminal event")
                .msg
            {
                EventMsg::TurnAborted(event)
                    if event.turn_id.as_deref() == Some(first_turn_id.as_str()) =>
                {
                    break;
                }
                EventMsg::TurnComplete(event) if event.turn_id == first_turn_id => {
                    panic!("aborted worker must not publish ordinary success: {event:?}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("aborted worker must terminalize");
    let rollout_path = thread.rollout_path().expect("real rollout path");
    let initial =
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(&rollout_path).await?;
    let persisted = initial
        .get_rollout_items()
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let live = session.clone_history().await;
    for (boundary, items) in [
        ("live history", live.raw_items()),
        ("physical rollout", persisted.as_slice()),
    ] {
        let observed = items
            .iter()
            .filter_map(|item| match item {
                ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } if call_id == "owned-result" => {
                    if boundary == "live history" {
                        assert_eq!(output.success, Some(true), "preserve actual tool success");
                    }
                    Some((
                        "output",
                        output.body.to_text().expect("actual tool result text"),
                    ))
                }
                ResponseItem::Message { role, content, .. } if role == "developer" => {
                    content.iter().find_map(|item| match item {
                        ContentItem::InputText { text } if text == post_context => {
                            Some(("context", text.clone()))
                        }
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![
                ("output", exact_output.to_string()),
                ("context", post_context.to_string())
            ],
            "{boundary}: one accepted result followed by its context must survive worker Drop"
        );
    }
    assert!(
        turn.pending_post_tool_contexts
            .lock()
            .await
            .get("owned-result")
            .is_none(),
        "the committed post-tool context must be consumed exactly once"
    );

    let next_turn = thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Use the completed probe result".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let EventMsg::TurnComplete(event) =
                thread.next_event().await.expect("continuation event").msg
                && event.turn_id == next_turn
            {
                assert!(event.error.is_none(), "{event:?}");
                break;
            }
        }
    })
    .await
    .expect("next normal turn consumes retained result");
    let captured = requests.requests();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[1]
            .function_call_output_content_and_success("owned-result")
            .expect("next request contains the function output")
            .0
            .as_deref(),
        Some(exact_output)
    );
    let input = captured[1].input();
    let output_indices = input
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item["type"] == "function_call_output" && item["call_id"] == "owned-result")
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let context_indices = input
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item["role"] == "developer"
                && item["content"]
                    .as_array()
                    .is_some_and(|content| content.iter().any(|item| item["text"] == post_context)))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        output_indices.len(),
        1,
        "next model request contains result exactly once"
    );
    assert_eq!(
        context_indices.len(),
        1,
        "next model request contains post-tool context exactly once"
    );
    assert!(output_indices[0] < context_indices[0]);
    thread.shutdown_and_wait().await?;
    manager.remove_thread(&started.thread_id).await;
    Ok(())
}

#[tokio::test]
async fn drain_in_flight_returns_first_error_after_draining_remaining_futures() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let remaining_future_polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let remaining_future_polled_clone = Arc::clone(&remaining_future_polled);
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "first",
            Box::pin(async { Err(CodexErr::Fatal("first tool failure".to_string())) }),
        )
        .into_future(),
    ));
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "second",
            Box::pin(async move {
                remaining_future_polled_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(CodexErr::Fatal("second tool failure".to_string()))
            }),
        )
        .into_future(),
    ));

    let error = drain_in_flight(&mut in_flight, Arc::clone(&session), Arc::new(turn_context))
        .await
        .expect_err("the first in-flight tool error should be returned");

    assert!(remaining_future_polled.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(
        error,
        CodexErr::Fatal(message) if message == "first tool failure"
    ));
    let history = session.clone_history().await;
    let failure_outputs = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } if call_id == "first" || call_id == "second" => {
                Some((call_id.as_str(), output.success))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        failure_outputs,
        vec![("first", Some(false)), ("second", Some(false))],
        "every accepted failed call must persist one ordered terminal output before the error escapes"
    );
}

#[tokio::test]
async fn drain_in_flight_persists_each_ordered_output_before_later_tools_finish() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let (release_second_tx, release_second_rx) = tokio::sync::oneshot::channel();
    let drain_session = Arc::clone(&session);
    let drain_turn_context = Arc::clone(&turn_context);
    let drain = tokio::spawn(async move {
        let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
            FuturesOrdered::new();
        in_flight.push_back(Box::pin(
            InFlightToolCall::from_test_future(
                "first",
                Box::pin(async { Ok(synthetic_tool_result("first")) }),
            )
            .into_future(),
        ));
        in_flight.push_back(Box::pin(
            InFlightToolCall::from_test_future(
                "second",
                Box::pin(async move {
                    let _ = release_second_rx.await;
                    Ok(synthetic_tool_result("second"))
                }),
            )
            .into_future(),
        ));
        drain_in_flight(&mut in_flight, drain_session, drain_turn_context).await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let history = session.clone_history().await;
            if history.raw_items().iter().any(|item| {
                matches!(
                    item,
                    ResponseItem::ToolSearchOutput { call_id, .. } if call_id.as_deref() == Some("first")
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first ordered output should be durable while a later tool is still running");

    release_second_tx
        .send(())
        .expect("the second tool should still be waiting");
    drain
        .await
        .expect("the drain task should join")
        .expect("both tool outputs should be delivered");
    let history = session.clone_history().await;
    let outputs = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::ToolSearchOutput { call_id, .. } => call_id.as_deref(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(outputs, vec!["first", "second"]);
}

#[tokio::test]
async fn drain_in_flight_commits_post_tool_context_after_its_output() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    turn_context
        .queue_post_tool_contexts(
            "annotated",
            vec![ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "post-tool context".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "annotated",
            Box::pin(async { Ok(synthetic_tool_result("annotated")) }),
        )
        .into_future(),
    ));

    drain_in_flight(
        &mut in_flight,
        Arc::clone(&session),
        Arc::clone(&turn_context),
    )
    .await
    .expect("the annotated output should be delivered");

    let history = session.clone_history().await;
    let ordered = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::ToolSearchOutput { call_id, .. }
                if call_id.as_deref() == Some("annotated") =>
            {
                Some("output")
            }
            ResponseItem::Message { role, content, .. }
                if role == "developer"
                    && content.iter().any(|item| {
                        matches!(item, ContentItem::InputText { text } if text == "post-tool context")
                    }) =>
            {
                Some("context")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ordered, vec!["output", "context"]);
}

#[test]
fn function_calls_select_argument_diff_consumers() {
    let item = ResponseItem::FunctionCall {
        id: None,
        name: "apply_patch".to_string(),
        namespace: Some("workspace".to_string()),
        arguments: String::new(),
        call_id: "function-diff".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };

    let (call_id, tool_name) = tool_argument_diff_target(&item)
        .expect("function tools should be eligible for argument diff streaming");

    assert_eq!(call_id, "function-diff");
    assert_eq!(
        tool_name,
        ToolName::new(Some("workspace".to_string()), "apply_patch")
    );
}

#[tokio::test]
async fn drain_in_flight_returns_earliest_required_terminal_after_persisting_all_outputs() {
    use crate::tools::context::RequiredToolTerminal;
    use crate::tools::context::RequiredToolTerminalCause;
    use crate::tools::parallel::ToolCallCompletion;

    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let mut first = InFlightToolCall::from_test_future(
        "first-terminal",
        Box::pin(async { Ok(synthetic_tool_result("first-terminal")) }),
    )
    .into_future()
    .await;
    first.result = Ok(ToolCallCompletion {
        failure_detail: None,
        response: synthetic_tool_result("first-terminal"),
        required_terminal: Some(RequiredToolTerminal {
            call_id: "first-terminal".to_string(),
            cause: RequiredToolTerminalCause::Failure,
            message: "first required failure".to_string(),
        }),
    });
    let mut second = InFlightToolCall::from_test_future(
        "second-terminal",
        Box::pin(async { Ok(synthetic_tool_result("second-terminal")) }),
    )
    .into_future()
    .await;
    second.result = Ok(ToolCallCompletion {
        failure_detail: None,
        response: synthetic_tool_result("second-terminal"),
        required_terminal: Some(RequiredToolTerminal {
            call_id: "second-terminal".to_string(),
            cause: RequiredToolTerminalCause::Blocked,
            message: "second required denial".to_string(),
        }),
    });
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(async move { first }));
    in_flight.push_back(Box::pin(async move { second }));

    let terminal = drain_in_flight(
        &mut in_flight,
        Arc::clone(&session),
        Arc::clone(&turn_context),
    )
    .await
    .expect("semantic failures are terminal results, not relay errors")
    .expect("a required terminal result should be returned");

    assert_eq!(terminal.call_id, "first-terminal");
    let history = session.clone_history().await;
    let persisted_call_ids = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::ToolSearchOutput { call_id, .. }
                if matches!(
                    call_id.as_deref(),
                    Some("first-terminal" | "second-terminal")
                ) =>
            {
                call_id.as_deref()
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_call_ids,
        vec!["first-terminal", "second-terminal"]
    );
    assert_eq!(
        turn_context
            .durable_history_completed_commits
            .lock()
            .await
            .len(),
        2,
        "each completed tool must commit its ordered output without waiting for later tools"
    );
}

#[tokio::test]
async fn drain_in_flight_attributes_duplicate_call_visibility_to_delivered_execution() {
    use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
    use crate::turn_timing::ToolCallTimingLineage;
    use codex_protocol::protocol::TurnTimingToolCallSource;

    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    turn_context.turn_timing_state.mark_turn_started();
    let mut calls = Vec::new();
    for _ in 0..2 {
        let mut call = InFlightToolCall::from_test_future(
            "duplicate-relay",
            Box::pin(async { Ok(synthetic_tool_result("duplicate-relay")) }),
        );
        call.timing = Arc::new(ToolDispatchTiming::new_with_turn_clock(
            Arc::clone(&turn_context.turn_timing_state),
            tokio::time::Instant::now(),
            false,
        ));
        call.execution_id = call.timing.execution_id().clone();
        calls.push(call.into_future().await);
    }
    let first = calls.remove(0);
    let later = calls.remove(0);
    let first_execution = first.execution_id.clone();
    let later_execution = later.execution_id.clone();
    // The later request completed first, but the relay must publish the earlier
    // request first. Keep the later completion queued outside this drain.
    for call in [&later, &first] {
        turn_context.turn_timing_state.record_tool_dispatch_timing(
            "duplicate-relay",
            "test_tool",
            TurnTimingToolCallSource::Direct,
            ToolCallTimingLineage::default(),
            call.timing.snapshot(tokio::time::Instant::now()),
        );
    }
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(async move { first }));
    drain_in_flight(
        &mut in_flight,
        Arc::clone(&session),
        Arc::clone(&turn_context),
    )
    .await
    .expect("the first relay output should commit");

    let history = session.clone_history().await;
    assert_eq!(
        history
            .raw_items()
            .iter()
            .filter(|item| matches!(
                item,
                ResponseItem::ToolSearchOutput { call_id, .. }
                    if call_id.as_deref() == Some("duplicate-relay")
            ))
            .count(),
        1,
    );
    let timing = turn_context
        .turn_timing_state
        .complete_snapshot()
        .protocol_timing();
    let delivered = timing
        .tool_calls
        .iter()
        .find(|call| call.execution_id == first_execution)
        .expect("first execution timing");
    let pending = timing
        .tool_calls
        .iter()
        .find(|call| call.execution_id == later_execution)
        .expect("later execution timing");
    assert!(delivered.delivered_at_ms.is_some());
    assert!(delivered.output_model_visible_at_ms.is_some());
    assert_eq!(pending.delivered_at_ms, None);
    assert_eq!(pending.output_model_visible_at_ms, None);
}

#[tokio::test]
async fn drain_in_flight_keeps_successful_delivery_independent_of_telemetry_state() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "successful",
            Box::pin(async { Ok(synthetic_tool_result("successful")) }),
        )
        .into_future(),
    ));

    drain_in_flight(&mut in_flight, Arc::clone(&session), Arc::new(turn_context))
        .await
        .expect("successful tool delivery must not depend on telemetry-only state");
    let history = session.clone_history().await;
    let actual = history
        .raw_items()
        .iter()
        .find(|item| {
            matches!(
                item,
                ResponseItem::ToolSearchOutput { call_id, .. }
                    if call_id.as_deref() == Some("successful")
            )
        })
        .expect("successful output must reach model-visible history");
    if let ResponseItem::ToolSearchOutput {
        status,
        execution,
        tools,
        omitted_result_count,
        ..
    } = actual
    {
        assert_eq!(status, "completed");
        assert_eq!(execution, "client");
        assert!(tools.is_empty());
        assert_eq!(*omitted_result_count, None);
    } else {
        panic!("expected tool search output");
    }
}

#[tokio::test]
async fn drain_in_flight_rollout_failure_does_not_attest_persistence_or_mutate_history() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    crate::session::tests::attach_thread_persistence(&mut session).await;
    session
        .live_thread()
        .expect("test session should have live persistence")
        .shutdown()
        .await
        .expect("test thread store should shut down");

    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let call_id = "durability-failure";
    let call = InFlightToolCall::from_test_future(
        call_id,
        Box::pin(async move { Ok(synthetic_tool_result(call_id)) }),
    );
    let execution_id = call.execution_id.clone();
    call.timing.record_outcome("success");
    turn_context.turn_timing_state.mark_turn_started();
    turn_context.turn_timing_state.record_accepted_tool_call(
        call_id,
        &execution_id,
        codex_protocol::protocol::TurnTimingToolCallSource::Direct,
        None,
    );
    turn_context.turn_timing_state.record_tool_dispatch_timing(
        call_id,
        "test_tool",
        codex_protocol::protocol::TurnTimingToolCallSource::Direct,
        crate::turn_timing::ToolCallTimingLineage::default(),
        call.timing.snapshot(tokio::time::Instant::now()),
    );
    let history_before = session.clone_history().await.raw_items().to_vec();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(call.into_future()));

    let error = drain_in_flight(
        &mut in_flight,
        Arc::clone(&session),
        Arc::clone(&turn_context),
    )
    .await
    .expect_err("rollout append failure must fail the relay before history mutation");

    assert!(matches!(
        error,
        CodexErr::Fatal(message) if message.contains("failed to durably append tool output")
    ));
    let history_after = session.clone_history().await;
    assert_eq!(history_after.raw_items(), history_before.as_slice());
    let closure = turn_context.turn_timing_state.tool_closure_snapshot();
    assert_eq!(
        (
            closure.accepted_count,
            closure.timing_paired_count,
            closure.terminal_count,
            closure.persisted_count,
        ),
        (1, 1, 1, 0)
    );
    assert_eq!(closure.unresolved_calls.len(), 1);
    assert_eq!(closure.unresolved_calls[0].call_id, call_id);
    assert_eq!(closure.unresolved_calls[0].execution_id, execution_id);
    assert!(!closure.complete);
}

#[tokio::test]
async fn drain_in_flight_persists_stale_relay_as_terminal_failure() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let mut stale = InFlightToolCall::from_test_future(
        "stale",
        Box::pin(async { Ok(synthetic_tool_result("stale")) }),
    )
    .into_future()
    .await;
    stale.execution_id = codex_protocol::protocol::ToolExecutionId("stale-execution".to_string());
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(async move { stale }));

    let error = drain_in_flight(&mut in_flight, Arc::clone(&session), Arc::new(turn_context))
        .await
        .expect_err("a stale relay must fail the turn after closing its accepted call");

    assert!(matches!(
        error,
        CodexErr::Fatal(message) if message.contains("stale tool relay completion for stale")
    ));
    let history = session.clone_history().await;
    let stale_outputs = history
        .raw_items()
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } if call_id == "stale" => Some(output.success),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stale_outputs,
        vec![Some(false)],
        "a stale accepted call must persist exactly one terminal output"
    );
}

#[tokio::test]
async fn steering_applies_next_turn_settings_without_building_a_candidate_turn_context() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let next_model = codex_models_manager::bundled_models_response()
        .expect("bundled model catalog should parse")
        .models
        .into_iter()
        .find(|model| model.slug != turn_context.model_info.slug)
        .expect("bundled model catalog should contain an alternative model")
        .slug;
    let active_approval_policy = turn_context.approval_policy.value();
    let active_permission_profile = turn_context.permission_profile.clone();
    let next_approval_policy = if active_approval_policy == AskForApproval::Never {
        AskForApproval::OnRequest
    } else {
        AskForApproval::Never
    };
    let next_permission_profile = if active_permission_profile == PermissionProfile::Disabled {
        PermissionProfile::read_only()
    } else {
        PermissionProfile::Disabled
    };
    session
        .services
        .thread_extension_data
        .insert(turn_context.model_info.clone());
    let model_info_before = session
        .services
        .thread_extension_data
        .get::<codex_protocol::openai_models::ModelInfo>()
        .expect("thread model info should be initialized");
    let finish = CancellationToken::new();
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            SignalCompletingTask {
                finish: finish.clone(),
            },
        )
        .await;

    crate::session::handlers::user_input_or_turn_inner(
        &session,
        "steering-submission".to_string(),
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "steer the active turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                approval_policy: Some(next_approval_policy),
                permission_profile: Some(next_permission_profile.clone()),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: next_model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        },
        /*client_user_message_id*/ None,
    )
    .await;

    let model_info_after = session
        .services
        .thread_extension_data
        .get::<codex_protocol::openai_models::ModelInfo>()
        .expect("thread model info should remain initialized");
    assert!(
        Arc::ptr_eq(&model_info_before, &model_info_after),
        "a successful steer must not build a candidate context or replace thread model metadata"
    );
    assert_eq!(session.collaboration_mode().await.model(), next_model);
    let active_context = session
        .turn_context_for_sub_id(&turn_context.sub_id)
        .await
        .expect("the original turn should remain active");
    assert!(Arc::ptr_eq(&active_context, &turn_context));
    assert_eq!(
        active_context.approval_policy.value(),
        active_approval_policy,
        "steering settings must not rebind the active turn's MCP approval policy"
    );
    assert_eq!(
        active_context.permission_profile, active_permission_profile,
        "steering settings must not rebind the active turn's MCP permission profile"
    );

    let terminal = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active_turn| active_turn.terminal.clone())
        .expect("active turn should expose its terminal coordinator");
    finish.cancel();
    terminal.wait_completed().await;

    let next_context = session
        .new_default_turn_with_sub_id("next-turn-after-steer".to_string())
        .await;
    assert_eq!(next_context.model_info.slug, next_model);
    assert_eq!(
        next_context.approval_policy.value(),
        next_approval_policy,
        "the next actual turn must install the steered MCP approval policy"
    );
    assert_eq!(
        next_context.permission_profile, next_permission_profile,
        "the next actual turn must install the steered MCP permission profile"
    );
}

#[test]
fn initial_response_item_triggers_compaction_before_the_stream_request() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "initial_response_item_triggers_compaction_before_the_stream_request",
        initial_response_item_triggers_compaction_before_the_stream_request_impl,
    )
}

async fn initial_response_item_triggers_compaction_before_the_stream_request_impl() -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("seed-response-item-response"),
                responses::ev_assistant_message("seed-response-item-message", "seed complete"),
                responses::ev_completed_with_tokens(
                    "seed-response-item-response",
                    /*total_tokens*/ 99_000,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("response-item-compact-response"),
                responses::ev_assistant_message(
                    "response-item-compact-message",
                    &complete_compaction_summary("initial response context compacted"),
                ),
                responses::ev_completed_with_tokens(
                    "response-item-compact-response",
                    /*total_tokens*/ 20,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("response-item-final-response"),
                responses::ev_assistant_message(
                    "response-item-final-message",
                    "initial response item sampled",
                ),
                responses::ev_completed_with_tokens(
                    "response-item-final-response",
                    /*total_tokens*/ 42,
                ),
            ]),
        ],
    )
    .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.model_context_window = Some(1_000_000);
        config.model_auto_compact_token_limit = Some(100_000);
        config.model_auto_compact_token_limit_scope =
            codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
    });
    let test = builder.build(&server).await?;

    test.submit_turn("seed committed history near the compaction limit")
        .await?;
    while let Ok(event) =
        tokio::time::timeout(Duration::from_millis(10), test.codex.next_event()).await
    {
        event.expect("seed turn event stream remains open");
    }
    test.codex
        .submit(Op::UserInput {
            items: Vec::new(),
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([(
                "phase-68-large-initial-response-item".to_string(),
                AdditionalContextEntry {
                    value: "large model-visible response context ".repeat(128),
                    kind: AdditionalContextKind::Application,
                },
            )]),
            thread_settings: Default::default(),
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(15), async {
        while request_log.requests().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the final sampling request should follow pre-turn compaction");
    let submitted_turn_id = request_log.requests()[2].body_json()["client_metadata"]["turn_id"]
        .as_str()
        .expect("final request turn id")
        .to_string();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match test.codex.next_event().await.expect("turn event").msg {
                EventMsg::Error(error) => {
                    panic!(
                        "response-item turn failed during pre-turn compaction: {}",
                        error.message
                    )
                }
                EventMsg::TurnComplete(turn) if submitted_turn_id == turn.turn_id => {
                    assert!(
                        turn.error.is_none(),
                        "response-item turn completed with an error: {:?}",
                        turn.error
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the response-item turn should complete after pre-turn compaction");

    let request_count = request_log.requests().len();
    assert_eq!(
        request_count, 3,
        "the large initial ResponseItem must trigger compaction before the turn's sampling request"
    );
    Ok(())
}

#[test]
fn oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit",
        oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit_impl,
    )
}

async fn oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit_impl()
-> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("seed-response"),
                responses::ev_assistant_message("seed-message", "seed complete"),
                responses::ev_completed_with_tokens("seed-response", /*total_tokens*/ 101_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("compact-response"),
                responses::ev_assistant_message(
                    "compact-message",
                    &complete_compaction_summary("oversized pending input compacted"),
                ),
                responses::ev_completed_with_tokens("compact-response", /*total_tokens*/ 20),
            ]),
            responses::sse(vec![
                responses::ev_response_created("final-response"),
                responses::ev_assistant_message("final-message", "pending input sampled"),
                responses::ev_completed_with_tokens("final-response", /*total_tokens*/ 42),
            ]),
        ],
    )
    .await;
    let provider = non_openai_model_provider(&server);
    let pending_plan_builds = Arc::new(AtomicUsize::new(0));
    let mut extension_builder = codex_extension_api::ExtensionRegistryBuilder::new();
    extension_builder.turn_input_contributor(Arc::new(CountingTurnInputContributor {
        poll_count: Arc::clone(&pending_plan_builds),
    }));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extension_builder.build()))
        .with_config(move |config| {
            config.model_provider = provider;
            config.model_context_window = Some(1_000_000);
            config.model_auto_compact_token_limit = Some(100_000);
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("seed committed history").await?;
    while let Ok(event) =
        tokio::time::timeout(Duration::from_millis(10), test.codex.next_event()).await
    {
        event.expect("seed turn event stream remains open");
    }
    pending_plan_builds.store(0, Ordering::SeqCst);
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "oversized pending payload ".repeat(128),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while request_log.requests().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the final sampling request should follow pending-input compaction");
    let submitted_turn_id = request_log.requests()[2].body_json()["client_metadata"]["turn_id"]
        .as_str()
        .expect("final request turn id")
        .to_string();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match test.codex.next_event().await.expect("turn event").msg {
                EventMsg::Error(error) => {
                    panic!(
                        "oversized pending-input turn failed during compaction: {}",
                        error.message
                    )
                }
                EventMsg::TurnComplete(turn) if submitted_turn_id == turn.turn_id => {
                    assert!(
                        turn.error.is_none(),
                        "oversized pending-input turn completed with an error: {:?}",
                        turn.error
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the pending-input turn should complete after compaction");

    assert_eq!(
        request_log.requests().len(),
        3,
        "the second turn should compact once, then sample instead of repeatedly compacting the same pending payload"
    );
    assert_eq!(
        pending_plan_builds.load(Ordering::SeqCst),
        2,
        "compaction should invalidate the initial pure projection and rebuild the pending plan against compacted history"
    );
    Ok(())
}

#[test]
fn pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot",
        pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot_impl,
    )
}

async fn pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot_impl() -> Result<()> {
    let command = crate::test_harness::stdio_server_bin()
        .context("test_stdio_server helper is required by the MCP inventory snapshot regression")?;
    let (mut session, mut turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    Arc::get_mut(&mut session)
        .expect("test session should be uniquely owned")
        .services
        .auth_manager = Arc::clone(&auth_manager);
    let turn = Arc::get_mut(&mut turn_context).expect("test turn should be uniquely owned");
    turn.auth_manager = Some(auth_manager);
    turn.model_info.supports_search_tool = false;
    let config = Arc::make_mut(&mut turn.config);
    config
        .features
        .enable(Feature::Apps)
        .expect("apps feature should be configurable in tests");
    let _ = config.features.disable(Feature::ToolSuggest);
    config.orchestrator_mcp_enabled = true;
    let mut servers = config.mcp_servers.get().clone();
    servers.insert(
        "snapshot".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command,
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(10)),
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );
    config
        .mcp_servers
        .set(servers)
        .expect("test MCP server configuration should be accepted");
    let refresh_config = config.clone();
    session
        .refresh_mcp_servers_now(turn_context.as_ref(), &refresh_config)
        .await;
    assert!(
        session
            .services
            .latest_mcp_runtime()
            .manager()
            .wait_for_server_ready("snapshot", Duration::from_secs(10))
            .await,
        "snapshot MCP server should become ready"
    );

    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    const SNAPSHOT_APP_ID: &str = "phase68-snapshot-app";
    const SNAPSHOT_APP_NAME: &str = "Phase 68 Snapshot App";
    const SNAPSHOT_TOOL_NAMESPACE: &str = "mcp__codex_apps__phase_68_snapshot_app";
    assert!(
        !step_context
            .mcp
            .manager()
            .list_all_tools()
            .await
            .iter()
            .any(|tool| tool.connector_id.as_deref() == Some(SNAPSHOT_APP_ID)),
        "the live manager inventory must intentionally differ from the seeded step snapshot"
    );
    step_context
        .seed_mcp_tools_for_test(vec![ToolInfo {
            server_name: CODEX_APPS_MCP_SERVER_NAME.to_string(),
            supports_parallel_tool_calls: false,
            server_origin: None,
            callable_name: "search".to_string(),
            callable_namespace: SNAPSHOT_TOOL_NAMESPACE.to_string(),
            namespace_description: None,
            tool: Tool::new_with_raw("search".to_string(), None, Arc::new(JsonObject::default())),
            connector_id: Some(SNAPSHOT_APP_ID.to_string()),
            connector_name: Some(SNAPSHOT_APP_NAME.to_string()),
            plugin_display_names: Vec::new(),
        }])
        .await;
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: format!("use [$snapshot](app://{SNAPSHOT_APP_ID})"),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    let cancellation_token = CancellationToken::new();
    let planning_generation = session.services.planning_generation();
    let PendingTurnPlanBuild::Ready(plan) = build_pure_pending_turn_plan(
        &session,
        Arc::clone(&step_context),
        &input,
        planning_generation,
        &cancellation_token,
    )
    .await?
    else {
        panic!("stable test inputs should produce a ready pending-turn plan");
    };
    assert!(
        plan.projected_prompt_pressure.total_tokens
            > estimate_pending_tokens(
                &input,
                &[],
                &[],
                plan.first_router.as_ref(),
                /*initial_context*/ true,
            )
            .total_tokens,
        "first-turn planning must account for full context before compaction"
    );
    assert!(plan.step_context.turn.apps_enabled());
    assert_eq!(
        plan.mentioned_apps,
        vec![(
            SNAPSHOT_APP_ID.to_string(),
            Some(SNAPSHOT_APP_NAME.to_string())
        )],
        "planning must resolve app mentions from the same seeded StepContext inventory as routing"
    );

    let (snapshot_ptr, snapshot_len) = {
        let tools = plan.step_context.mcp_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].connector_id.as_deref(), Some(SNAPSHOT_APP_ID));
        (tools.as_ptr(), tools.len())
    };
    let cached_tools = plan.step_context.mcp_tools().await;
    assert_eq!(cached_tools.as_ptr(), snapshot_ptr);
    assert_eq!(cached_tools.len(), snapshot_len);
    let router_tool_names = plan
        .first_router
        .model_visible_specs()
        .iter()
        .map(|spec| spec.name().to_string())
        .collect::<Vec<_>>();
    assert!(
        router_tool_names.iter().any(|name| {
            name == SNAPSHOT_TOOL_NAMESPACE || name == &format!("{SNAPSHOT_TOOL_NAMESPACE}.search")
        }),
        "the advertised router must be built from the seeded StepContext inventory; expected namespace {SNAPSHOT_TOOL_NAMESPACE:?}, got {router_tool_names:?}"
    );
    Ok(())
}

fn wait_for_concurrent_state_attempt(attempted: &std::sync::atomic::AtomicBool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !attempted.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        attempted.load(Ordering::Acquire),
        "concurrent state attempt did not start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_plan_commit_and_invalidation_share_the_session_state_owner() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let stale_generation = session.services.planning_generation();
    let prepared_context_update = session.prepare_context_update(step_context.as_ref()).await;
    let history_before = session.clone_history().await.into_raw_items();
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );

    // Invalidation owns SessionState first. The pending commit cannot perform
    // its final comparison until after generation N has been invalidated.
    let commit = {
        let mut state_owner = session.state.lock().await;
        let commit_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let commit_attempted_task = Arc::clone(&commit_attempted);
        let commit_session = Arc::clone(&session);
        let commit = tokio::spawn(async move {
            commit_attempted_task.store(true, Ordering::Release);
            commit_session
                .compare_and_record_context_updates(prepared_context_update, stale_generation)
                .await
                .unwrap()
        });
        wait_for_concurrent_state_attempt(&commit_attempted);
        assert!(!commit.is_finished());
        session
            .services
            .advance_planning_generation(&mut state_owner);
        commit
    };
    assert!(commit.await.expect("commit task completed").is_none());
    assert_eq!(
        session.clone_history().await.into_raw_items(),
        history_before
    );
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );

    // Prepare a real context candidate, then replay the exact synchronous
    // compare-and-commit primitive while retaining SessionState ownership.
    let current_generation = session.services.planning_generation();
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let prepared_context_update = session.prepare_context_update(step_context.as_ref()).await;
    assert!(
        session
            .compare_and_record_context_updates(prepared_context_update, current_generation)
            .await
            .unwrap()
            .is_some()
    );
    let invalidation = {
        let mut state_owner = session.state.lock().await;
        let candidate = state_owner
            .pending_context_baseline()
            .expect("successful context commit stages its baseline");
        state_owner.clear_pending_context_baseline();
        let marker = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let history_len_before_commit = state_owner.clone_history().into_raw_items().len();
        let invalidation_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invalidation_attempted_task = Arc::clone(&invalidation_attempted);
        let invalidation_session = Arc::clone(&session);
        let invalidation = tokio::spawn(async move {
            invalidation_attempted_task.store(true, Ordering::Release);
            let mut state_owner = invalidation_session.state.lock().await;
            invalidation_session
                .services
                .advance_planning_generation(&mut state_owner)
        });
        wait_for_concurrent_state_attempt(&invalidation_attempted);
        assert!(!invalidation.is_finished());

        assert!(
            session
                .compare_and_commit_planning_state(
                    &mut state_owner,
                    Some(current_generation),
                    |state| {
                        state.record_items(
                            std::iter::once(&marker),
                            turn_context.model_info.truncation_policy.into(),
                        );
                        state.stage_context_baseline(candidate);
                    },
                )
                .is_some()
        );
        assert_eq!(
            state_owner.clone_history().into_raw_items().len(),
            history_len_before_commit + 1
        );
        assert!(state_owner.pending_context_baseline().is_some());
        assert!(!invalidation.is_finished());
        invalidation
    };

    let next_generation = invalidation.await.expect("invalidation task completed");
    assert!(next_generation > current_generation);
}

#[tokio::test]
async fn realized_context_commits_only_the_bound_physical_attempt() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    session
        .record_context_updates_and_set_reference_context_item(step_context.as_ref())
        .await
        .unwrap();

    assert!(
        session
            .bind_context_baseline_candidate("request-1", "attempt-1")
            .await
            .0
    );
    assert!(
        session
            .bind_context_baseline_candidate("request-1", "attempt-2")
            .await
            .0
    );
    assert!(
        !session
            .commit_context_baseline_candidate("request-1", "attempt-1")
            .await
            .expect("stale attempt must be ignored")
    );
    assert!(session.reference_context_item().await.is_none());

    assert!(
        session
            .commit_context_baseline_candidate("request-1", "attempt-2")
            .await
            .expect("matching attempt commits")
    );
    let realized = session
        .reference_context_item()
        .await
        .expect("matching accepted attempt realizes context");
    let provenance = realized
        .context_provenance
        .expect("realized context records accepted attempt provenance");
    assert_eq!(provenance.accepted_attempt.sampling_request_id, "request-1");
    assert_eq!(provenance.accepted_attempt.physical_attempt_id, "attempt-2");
    assert!(!provenance.fragment_digests.is_empty());
    assert_eq!(
        session
            .bind_context_baseline_candidate("request-2", "attempt-3")
            .await,
        (false, false),
        "unchanged accepted context is not an unresolved boundary"
    );
    assert!(
        !session
            .commit_context_baseline_candidate("request-1", "attempt-2")
            .await
            .expect("duplicate Created must be ignored")
    );
}

#[tokio::test]
async fn pending_plan_rebuilds_after_generation_changes_during_planning() -> Result<()> {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let history_before = session.clone_history().await.into_raw_items();
    let planning_generation = session.services.planning_generation();
    let stale_step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    {
        let mut state_owner = session.state.lock().await;
        session
            .services
            .advance_planning_generation(&mut state_owner);
    }

    let build = build_pure_pending_turn_plan(
        &session,
        stale_step_context,
        &[],
        planning_generation,
        &CancellationToken::new(),
    )
    .await?;
    assert!(matches!(build, PendingTurnPlanBuild::Stale));
    assert_eq!(
        session.clone_history().await.into_raw_items(),
        history_before
    );

    let rebuilt_generation = session.services.planning_generation();
    let rebuilt_step_context = session.capture_step_context(turn_context).await.unwrap();
    let rebuilt = build_pure_pending_turn_plan(
        &session,
        rebuilt_step_context,
        &[],
        rebuilt_generation,
        &CancellationToken::new(),
    )
    .await?;
    let PendingTurnPlanBuild::Ready(rebuilt) = rebuilt else {
        panic!("a plan rebuilt from the current generation should be ready");
    };
    assert_eq!(rebuilt.planning_generation, rebuilt_generation);
    Ok(())
}

#[test]
fn pending_token_estimate_includes_model_visible_tool_schemas() {
    let empty_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let empty_router = ToolRouter::from_parts(empty_registry, Vec::new());
    let schema_description = "schema context ".repeat(1024);
    let schema_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let schema_router = ToolRouter::from_parts(
        schema_registry,
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "large_schema_tool".to_string(),
                description: schema_description,
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::object(
                    Default::default(),
                    None,
                    Some(false.into()),
                ),
                output_schema: None,
            },
        )],
    );

    let baseline =
        estimate_pending_tokens(&[], &[], &[], &empty_router, /*initial_context*/ false);
    let with_schema = estimate_pending_tokens(
        &[],
        &[],
        &[],
        &schema_router,
        /*initial_context*/ false,
    );

    assert!(
        with_schema.total_tokens > baseline.total_tokens + 3_000,
        "model-visible schema bytes must materially increase pre-turn context estimation"
    );
    assert_eq!(
        with_schema.body_growth_tokens, baseline.body_growth_tokens,
        "stable tool schemas must not count as body-after-prefix growth"
    );
}

#[test]
fn pending_token_estimate_only_forecasts_real_user_boundaries() {
    let router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    );
    let user = ResponseItem::from(ResponseInputItem::from(vec![UserInput::Text {
        text: "continue".to_string(),
        text_elements: Vec::new(),
    }]));
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").unwrap(),
        Vec::new(),
        "continue".to_string(),
        true,
    );
    for (input, expected) in [
        (
            TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "continue".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
            true,
        ),
        (
            TurnInput::UserInput {
                content: Vec::new(),
                client_id: None,
            },
            false,
        ),
        (TurnInput::ResponseItem(user.clone()), true),
        (TurnInput::InternalResponseItem(user), false),
        (
            TurnInput::ResponseItem(communication.to_model_input_item()),
            false,
        ),
        (TurnInput::InterAgentCommunication(communication), false),
    ] {
        assert_eq!(
            estimate_pending_tokens(&[input], &[], &[], &router, false).has_real_user_boundary,
            expected
        );
    }
    let contextual = ContextualUserFragment::into(crate::context::UserInstructions {
        directory: None,
        text: "startup context".to_string(),
    });
    assert!(
        !estimate_pending_tokens(
            &[TurnInput::ResponseItem(contextual)],
            &[],
            &[],
            &router,
            false
        )
        .has_real_user_boundary
    );
}

#[tokio::test]
async fn accountingless_completion_preserves_sampling_reasoning_pressure() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let user = |text: &str| {
        ResponseItem::from(ResponseInputItem::from(vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }]))
    };
    let items = vec![
        user("task"),
        ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: Some("a".repeat(4_000)),
            internal_chat_message_metadata_passthrough: None,
        },
        user("continue"),
    ];
    session
        .record_conversation_items(&turn, &items)
        .await
        .unwrap();
    session.record_token_usage_info(&turn, None).await.unwrap();
    let expected = items
        .iter()
        .map(crate::context_manager::estimate_item_token_count)
        .fold(
            codex_utils_output_truncation::approx_token_count(&turn.base_instructions.text) as i64,
            i64::saturating_add,
        );
    assert_eq!(
        session.get_estimated_token_count(&turn).await,
        Some(expected)
    );
    let info = session.token_usage_info().await.unwrap();
    assert_eq!(info.estimated_context_tokens, Some(expected));
    assert_eq!(info.last_token_usage.total_tokens, 0);
    assert_eq!(info.total_token_usage.total_tokens, 0);
    let pending = PendingTokenEstimate {
        total_tokens: 20,
        body_growth_tokens: 10,
        has_real_user_boundary: false,
    };
    assert_eq!(
        projected_prompt_pressure(&session, &turn, pending)
            .await
            .total_tokens,
        expected + 20
    );
}

#[test]
fn pending_token_estimate_excludes_stable_startup_injections_from_body_growth() {
    let empty_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let empty_router = ToolRouter::from_parts(empty_registry, Vec::new());
    let baseline =
        estimate_pending_tokens(&[], &[], &[], &empty_router, /*initial_context*/ false);
    let instructions = ContextualUserFragment::into(crate::context::UserInstructions {
        directory: None,
        text: "Keep the task focused.".to_string(),
    });
    let with_instructions = estimate_pending_tokens(
        &[],
        &[instructions],
        &[],
        &empty_router,
        /*initial_context*/ false,
    );

    assert!(with_instructions.total_tokens > baseline.total_tokens);
    assert_eq!(
        with_instructions.body_growth_tokens,
        baseline.body_growth_tokens
    );
}

#[test]
fn audit_reports_17_19_pending_images_use_modality_estimates() {
    let router = ToolRouter::from_parts(
        crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    );
    let image = |bytes: usize| {
        ResponseItem::from(ResponseInputItem::from(vec![UserInput::Image {
            image_url: format!("data:image/png;base64,{}", "a".repeat(bytes)),
            detail: None,
        }]))
    };
    let short = estimate_pending_tokens(&[], &[image(100)], &[], &router, false);
    let long = estimate_pending_tokens(&[], &[image(4 * 1024 * 1024)], &[], &router, false);
    assert_eq!(short.total_tokens, long.total_tokens);
    assert_eq!(short.body_growth_tokens, long.body_growth_tokens);
    assert!(long.total_tokens > 1_800 && long.total_tokens < 2_000);
    for content in [
        UserInput::Image {
            image_url: format!("data:image/png;base64,{}", "a".repeat(4 * 1024 * 1024)),
            detail: None,
        },
        UserInput::Image {
            image_url: "https://example.invalid/image.png".to_string(),
            detail: None,
        },
        UserInput::LocalImage {
            path: PathBuf::from("nonexistent-image.png"),
            detail: None,
        },
    ] {
        let pending = [TurnInput::UserInput {
            content: vec![content],
            client_id: None,
        }];
        let estimate = estimate_pending_tokens(&pending, &[], &[], &router, false);
        assert!(estimate.total_tokens > 1_800 && estimate.total_tokens < 2_000);
        assert!(estimate.body_growth_tokens > 1_800);
    }
    let text = ResponseItem::from(ResponseInputItem::from(vec![UserInput::Text {
        text: "a".repeat(40_000),
        text_elements: Vec::new(),
    }]));
    assert!(estimate_pending_tokens(&[], &[text], &[], &router, false).total_tokens > 10_000);
}

#[test]
fn stop_hook_continuation_reaches_the_final_response() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "stop_hook_continuation_reaches_the_final_response",
        stop_hook_continuation_reaches_the_final_response_impl,
    )
}

#[cfg(windows)]
#[test]
fn registered_exec_normalizes_yield_and_runs_the_command() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "registered_exec_normalizes_yield_and_runs_the_command",
        registered_exec_normalizes_yield_and_runs_the_command_impl,
    )
}

#[cfg(windows)]
async fn registered_exec_normalizes_yield_and_runs_the_command_impl() -> Result<()> {
    let fixtures = tempfile::tempdir()?;
    let marker = fixtures.path().join("command-ran.txt");
    let script = format!(
        "Set-Content -LiteralPath '{}' -Value ran",
        marker.to_string_lossy().replace('\'', "''")
    );
    let server = responses::start_mock_server().await;
    let response_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("invalid-yield-response"),
                responses::ev_function_call(
                    "invalid-yield",
                    "exec_command",
                    &serde_json::json!({
                        "kind": "argv",
                        "program": "powershell.exe",
                        "args": ["-NoProfile", "-NonInteractive", "-Command", script],
                        "yield_time_ms": 300_001,
                    })
                    .to_string(),
                ),
                responses::ev_completed("invalid-yield-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("invalid-yield-final-response"),
                responses::ev_assistant_message(
                    "invalid-yield-final-message",
                    "observation wait normalized",
                ),
                responses::ev_completed("invalid-yield-final-response"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("register exec_command");
    });
    let test = builder.build(&server).await?;
    let completion = test
        .submit_turn_and_capture_completion("run the supplied command")
        .await?;
    assert!(completion.error.is_none(), "{completion:?}");
    let requests = response_log.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1]
        .function_call_output_text("invalid-yield")
        .expect("model receives command completion and normalization notice");
    assert!(
        output.contains("Adjusted `yield_time_ms` to 300000"),
        "{output}"
    );
    assert_eq!(std::fs::read_to_string(marker)?.trim(), "ran");
    Ok(())
}

#[cfg(windows)]
#[test]
fn registered_command_repair_preserves_safety_history_and_post_hook_context() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "registered_command_repair_preserves_safety_history_and_post_hook_context",
        registered_command_repair_preserves_safety_history_and_post_hook_context_impl,
    )
}

#[cfg(windows)]
async fn registered_command_repair_preserves_safety_history_and_post_hook_context_impl()
-> Result<()> {
    let fixtures = tempfile::tempdir()?;
    let input_path = fixtures.path().join("input.txt");
    let helper_path = fixtures.path().join("search-helper.cmd");
    let helper_marker = fixtures.path().join("helper-ran.txt");
    // Produce enough real output to exercise durable completed-tool history.
    let input_contents = "todo: registered search result\n".repeat(256);
    fs::write(&input_path, &input_contents)?;
    fs::write(
        &helper_path,
        "@echo off\r\n> \"%~dp0helper-ran.txt\" echo ran\r\ntype \"%~1\"\r\n",
    )?;
    let server = responses::start_mock_server().await;
    let response_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("unsafe-search-response"),
                responses::ev_function_call(
                    "unsafe-search",
                    "exec_command",
                    &serde_json::json!({
                        "kind": "argv", "program": "rg",
                        "args": ["--ignorecase", "--pre", helper_path, "TODO", input_path],
                        "yield_time_ms": 1000,
                    })
                    .to_string(),
                ),
                responses::ev_completed("unsafe-search-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("safe-search-response"),
                responses::ev_function_call(
                    "safe-search",
                    "exec_command",
                    &serde_json::json!({
                        "kind": "argv", "program": "rg",
                        "args": ["--ignorecase", "TODO", input_path],
                        "yield_time_ms": 1000,
                    })
                    .to_string(),
                ),
                responses::ev_completed("safe-search-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("search-final-response"),
                responses::ev_assistant_message("search-final-message", "search complete"),
                responses::ev_completed("search-final-response"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            let script_path = home.join("post_search_hook.py");
            fs::write(
                &script_path,
                concat!(
                    "import json, sys\n",
                    "json.load(sys.stdin)\n",
                    "print(json.dumps({'hookSpecificOutput': {'hookEventName': 'PostToolUse', ",
                    "'additionalContext': 'registered search post-hook context'}}))\n",
                ),
            )
            .expect("write post-tool hook");
            fs::write(
                home.join("hooks.json"),
                serde_json::json!({
                    "hooks": {"PostToolUse": [{"matcher": "Bash", "hooks": [{
                        "type": "command",
                        "command": format!("python \"{}\"", script_path.display()),
                    }]}]},
                })
                .to_string(),
            )
            .expect("write hook registration");
        })
        .with_config(|config| {
            config
                .features
                .disable(Feature::DirectRuntime)
                .expect("configure legacy repair");
            config
                .features
                .enable(Feature::UnifiedExec)
                .expect("register exec_command");
            config
                .features
                .enable(Feature::CodexHooks)
                .expect("enable registered hooks");
            config.completed_tool_history_projection = true;
            trust_discovered_hooks(config);
        });
    let test = builder.build(&server).await?;
    let completion = test
        .submit_turn_and_capture_completion("search for TODO in the supplied file")
        .await?;
    assert!(completion.error.is_none(), "{completion:?}");
    let requests = response_log.requests();
    assert_eq!(requests.len(), 3);
    let rejected = requests[1]
        .function_call_output_text("unsafe-search")
        .expect("model receives rejected command result");
    assert!(rejected.contains("--ignorecase"), "{rejected}");
    assert!(
        !rejected.contains("read-only equivalent repair"),
        "{rejected}"
    );
    assert!(
        !helper_marker.exists(),
        "rejected repair must not launch its search helper"
    );
    let output = requests[2]
        .function_call_output_text("safe-search")
        .expect("model receives real command output");
    assert!(
        output.contains("todo: registered search result"),
        "{output}"
    );
    assert!(output.contains("read-only equivalent repair"), "{output}");
    let inputs = requests[2].input();
    let output_index = inputs
        .iter()
        .position(|item| item["call_id"] == "safe-search" && item["type"] == "function_call_output")
        .expect("completed tool output");
    let context_indices = inputs
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item["type"] == "message"
                && item
                    .to_string()
                    .contains("registered search post-hook context"))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        context_indices.len(),
        1,
        "the registered hook context must be delivered once"
    );
    assert!(
        context_indices[0] > output_index,
        "post-tool context must follow its tool output"
    );

    test.codex.submit(Op::Shutdown).await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if matches!(
                test.codex.next_event().await.expect("shutdown event").msg,
                EventMsg::ShutdownComplete
            ) {
                break;
            }
        }
    })
    .await
    .expect("shutdown flushes completed-tool history");
    let (persisted, warning) = crate::tool_history::load_tool_history_state(
        test.codex_home_path(),
        &test.session_configured.thread_id.to_string(),
    )
    .await
    .into_state_and_warning();
    assert_eq!(warning, None);
    let persisted = serde_json::to_value(persisted)?;
    assert_eq!(persisted["candidates"]["safe-search"]["successful"], true);
    assert!(
        persisted["candidates"]["safe-search"]["bounded_digest"]
            .as_str()
            .expect("persisted result")
            .contains("todo: registered search result")
    );
    assert_eq!(fs::read_to_string(input_path)?, input_contents);
    Ok(())
}

async fn stop_hook_continuation_reaches_the_final_response_impl() -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let response_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("plan-response"),
                responses::ev_function_call(
                    "plan-call",
                    "update_plan",
                    &serde_json::json!({
                        "plan": [{
                            "step": "exercise stop-hook continuation",
                            "status": "in_progress"
                        }]
                    })
                    .to_string(),
                ),
                responses::ev_completed("plan-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("draft-response"),
                responses::ev_assistant_message("draft-message", "draft answer"),
                responses::ev_completed("draft-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("final-response"),
                responses::ev_assistant_message("final-message", "final answer"),
                responses::ev_completed("final-response"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            write_one_shot_stop_hook(home).expect("write stop-hook fixture");
        })
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "answer, then continue after the stop hook".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let mut saw_final_response = false;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = test.codex.next_event().await.expect("turn event");
            match event.msg {
                EventMsg::AgentMessage(message) if message.message == "final answer" => {
                    saw_final_response = true;
                }
                EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("turn should finish after one stop-hook continuation");
    assert!(saw_final_response);
    assert_eq!(response_log.requests().len(), 3);
    Ok(())
}

#[test]
fn models_etag_refresh_does_not_block_tool_continuation() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "models_etag_refresh_does_not_block_tool_continuation",
        models_etag_refresh_does_not_block_tool_continuation_impl,
    )
}

async fn models_etag_refresh_does_not_block_tool_continuation_impl() -> Result<()> {
    crate::test_harness::require_network!();
    const REFRESH_ETAG: &str = "\"phase-68-models-2\"";

    let server = responses::start_mock_server().await;
    let mut builder = test_codex()
        .with_remote_models()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model("gpt-5.2")
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            let _ = config.features.disable(Feature::Apps);
        });
    let test = builder.build(&server).await?;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .insert_header("etag", REFRESH_ETAG)
                .set_body_json(ModelsResponse { models: Vec::new() }),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let response_log = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("etag-tool-response"),
                responses::ev_function_call(
                    "etag-plan-call",
                    "update_plan",
                    &serde_json::json!({
                        "plan": [{
                            "id": "etag-continuation",
                            "step": "continue while the model catalog refresh is pending",
                            "status": "implemented",
                            "acceptance_criteria": ["the second model request is dispatched"],
                            "runtime_paths": ["core/src/session/turn.rs"]
                        }]
                    })
                    .to_string(),
                ),
                responses::ev_completed("etag-tool-response"),
            ]))
            .insert_header("X-Models-Etag", REFRESH_ETAG),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("etag-final-response"),
                responses::ev_assistant_message(
                    "etag-final-message",
                    "tool continuation completed",
                ),
                responses::ev_completed("etag-final-response"),
            ])),
        ],
    )
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "exercise ETag refresh during a tool continuation".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let model_requests = server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|request| request.url.path() == "/v1/models")
                .count();
            if model_requests >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background models refresh should start");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = test.codex.next_event().await.expect("stream event");
            match event.msg {
                EventMsg::AgentMessage(ref message)
                    if message.message == "tool continuation completed" =>
                {
                    break;
                }
                EventMsg::TurnComplete(_) => {
                    panic!("turn completed without the continuation response")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the second model request must not wait for the delayed models refresh");
    assert_eq!(response_log.requests().len(), 2);
    Ok(())
}

#[tokio::test]
async fn unchanged_model_and_comp_hash_skip_previous_model_context_reconstruction() -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ModelsResponse { models: Vec::new() }),
        )
        .mount(&server)
        .await;

    let (mut session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut config = (*turn_context.config).clone();
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.model_provider.supports_websockets = false;
    let config = Arc::new(config);
    session.services.auth_manager = Arc::clone(&auth_manager);
    session.services.models_manager = crate::test_support::models_manager_with_provider(
        config.codex_home.to_path_buf(),
        Arc::clone(&auth_manager),
        config.model_provider.clone(),
    );
    turn_context.auth_manager = Some(auth_manager);
    turn_context.config = config;
    session
        .set_previous_turn_settings(Some(crate::session::PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: turn_context.model_info.comp_hash.clone(),
        }))
        .await;
    let mut client_session = session.services.model_client.new_session();
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);

    assert!(
        !maybe_run_previous_model_inline_compact(
            &session,
            &turn_context,
            &mut client_session,
            &CancellationToken::new(),
        )
        .await?
    );
    let model_requests = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/models")
        .count();
    assert_eq!(
        model_requests, 0,
        "unchanged settings should return before TurnContext::with_model fetches the catalog"
    );
    Ok(())
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
        &mut None,
        &mut None,
    )
    .await
    .unwrap();

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

fn synthetic_tool_result(call_id: &str) -> ResponseInputItem {
    ResponseInputItem::ToolSearchOutput {
        call_id: call_id.to_string(),
        status: "completed".to_string(),
        execution: "client".to_string(),
        tools: Vec::new(),
        omitted_result_count: None,
    }
}

fn controlled_tool_future(
    call_id: &'static str,
    first_poll: tokio::sync::oneshot::Sender<tokio::time::Instant>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> BoxFuture<'static, CodexResult<ResponseInputItem>> {
    Box::pin(async move {
        let _ = first_poll.send(tokio::time::Instant::now());
        let _ = release.await;
        Ok(synthetic_tool_result(call_id))
    })
}

fn controlled_tool_call(
    call_id: &'static str,
    first_poll: tokio::sync::oneshot::Sender<tokio::time::Instant>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> InFlightToolCall {
    InFlightToolCall::from_test_future(
        call_id,
        controlled_tool_future(call_id, first_poll, release),
    )
}

// This polls the future itself; it does not drive the sampling loop that
// decides when an accepted call is first polled.
#[tokio::test]
async fn in_flight_tool_call_future_relays_its_result_once_polled() {
    let (first_poll_tx, first_poll_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(
        controlled_tool_call("streamed", first_poll_tx, release_rx)
            .into_future()
            .boxed(),
    );
    let result_task = tokio::spawn(async move { in_flight.next().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), first_poll_rx)
        .await
        .expect("a polled call must start its handler")
        .expect("tool handler started");
    release_tx.send(()).unwrap();
    assert_eq!(
        result_task.await.unwrap().result.unwrap().response,
        synthetic_tool_result("streamed"),
    );
}

#[tokio::test(start_paused = true)]
async fn eligible_direct_tool_calls_overlap_a_response_tail_without_changing_results() {
    const RESPONSE_TAIL: Duration = Duration::from_millis(250);
    let baseline_item_accepted = tokio::time::Instant::now();
    let (baseline_first_poll_tx, mut baseline_first_poll_rx) = tokio::sync::oneshot::channel();
    let (baseline_release_tx, baseline_release_rx) = tokio::sync::oneshot::channel();
    let mut baseline: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    baseline.push_back(controlled_tool_future(
        "read-1",
        baseline_first_poll_tx,
        baseline_release_rx,
    ));

    tokio::task::yield_now().await;
    assert!(matches!(
        baseline_first_poll_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    tokio::time::advance(RESPONSE_TAIL).await;
    let baseline_result_task =
        tokio::spawn(async move { baseline.next().await.expect("baseline result should exist") });
    let baseline_first_poll = baseline_first_poll_rx
        .await
        .expect("baseline future should be polled");
    assert_eq!(
        baseline_first_poll.duration_since(baseline_item_accepted),
        RESPONSE_TAIL
    );
    baseline_release_tx
        .send(())
        .expect("baseline tool should still be attached");
    let baseline_result = baseline_result_task
        .await
        .expect("baseline result task should finish")
        .expect("baseline tool should succeed");

    let eager_item_accepted = tokio::time::Instant::now();
    let (eager_first_poll_tx, eager_first_poll_rx) = tokio::sync::oneshot::channel();
    let (eager_release_tx, eager_release_rx) = tokio::sync::oneshot::channel();
    let mut eager: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> = FuturesOrdered::new();
    eager.push_back(start_eager_tool_future(controlled_tool_call(
        "read-1",
        eager_first_poll_tx,
        eager_release_rx,
    )));

    let eager_first_poll = eager_first_poll_rx
        .await
        .expect("eager future should start before the response tail is released");
    assert_eq!(
        eager_first_poll.duration_since(eager_item_accepted),
        Duration::ZERO
    );
    let (stream_tail_completed_tx, stream_tail_completed_rx) = tokio::sync::oneshot::channel();
    let stream_tail = tokio::spawn(async move {
        tokio::time::sleep(RESPONSE_TAIL).await;
        let _ = stream_tail_completed_tx.send(tokio::time::Instant::now());
    });
    tokio::task::yield_now().await;
    tokio::time::advance(RESPONSE_TAIL).await;
    let eager_tail_completed = stream_tail_completed_rx
        .await
        .expect("the model-response tail should continue while the tool is blocked");
    stream_tail.await.expect("stream-tail task should finish");
    assert_eq!(
        eager_tail_completed.duration_since(eager_first_poll),
        RESPONSE_TAIL
    );

    // The simulated stream tail completed while tool work was still deliberately blocked.
    // Model continuation remains behind the ordered tool-result barrier.
    let next_sampling_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let next_sampling_started_after_drain = Arc::clone(&next_sampling_started);
    let continuation = tokio::spawn(async move {
        let result = eager
            .next()
            .await
            .expect("eager result should exist")
            .result
            .expect("eager tool should succeed")
            .response;
        next_sampling_started_after_drain.store(true, Ordering::SeqCst);
        result
    });
    tokio::task::yield_now().await;
    assert!(!next_sampling_started.load(Ordering::SeqCst));
    eager_release_tx
        .send(())
        .expect("eager tool should still be attached");
    let eager_result = continuation
        .await
        .expect("continuation barrier task should finish");

    assert_eq!(eager_result, baseline_result);
    assert!(next_sampling_started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn eager_tool_results_remain_in_call_order_after_reverse_completion() {
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
    let (first_release_tx, first_release_rx) = tokio::sync::oneshot::channel();
    let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
    let (second_release_tx, second_release_rx) = tokio::sync::oneshot::channel();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(controlled_tool_call(
        "first",
        first_started_tx,
        first_release_rx,
    )));
    in_flight.push_back(start_eager_tool_future(controlled_tool_call(
        "second",
        second_started_tx,
        second_release_rx,
    )));

    first_started_rx.await.expect("first tool should start");
    second_started_rx.await.expect("second tool should start");
    second_release_tx
        .send(())
        .expect("second tool should still be attached");
    tokio::task::yield_now().await;
    first_release_tx
        .send(())
        .expect("first tool should still be attached");

    let first = in_flight
        .next()
        .await
        .expect("first result")
        .result
        .expect("success")
        .response;
    let second = in_flight
        .next()
        .await
        .expect("second result")
        .result
        .expect("success")
        .response;
    assert_eq!(first, synthetic_tool_result("first"));
    assert_eq!(second, synthetic_tool_result("second"));
}

#[tokio::test(start_paused = true)]
async fn eager_tool_failure_can_be_collected_before_response_streaming_finishes() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let future: BoxFuture<'static, CodexResult<ResponseInputItem>> = Box::pin(async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
        Err(CodexErr::Fatal("synthetic eager tool failure".to_string()))
    });
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(InFlightToolCall::from_test_future(
        "failure", future,
    )));

    started_rx.await.expect("eager tool should start");
    let (tail_finished_tx, mut tail_finished_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = tail_finished_tx.send(());
    });

    release_tx
        .send(())
        .expect("failed eager tool should remain attached until collection");
    let error = in_flight
        .next()
        .await
        .expect("failed result should retain its ordered slot")
        .result
        .expect_err("synthetic tool should fail");
    assert!(error.to_string().contains("synthetic eager tool failure"));
    assert!(matches!(
        tail_finished_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
}

struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
async fn dropping_in_flight_collection_aborts_eager_tool_work() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let future: BoxFuture<'static, CodexResult<ResponseInputItem>> = Box::pin(async move {
        let _drop_signal = DropSignal(Some(dropped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
        unreachable!("aborted eager work must not resume")
    });
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(InFlightToolCall::from_test_future(
        "drop", future,
    )));

    started_rx.await.expect("eager work should start");
    drop(in_flight);
    dropped_rx
        .await
        .expect("dropping the collection must abort, not detach, eager work");
}
#[test]
fn terminal_surface_preserves_typed_owner_result_without_json_message_synthesis() {
    let value = serde_json::json!({"status": "complete", "nested": {"answer": 42}});
    let decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            AuthoritativeWaitOwnerResult {
                adapter: "owner_adapter".to_string(),
                value: value.clone(),
                surfaceable_message: None,
            },
        )),
        ..Default::default()
    };

    let surfaced =
        authoritative_wait_terminal_surface(&decision, None, 10_000).expect("typed surface");
    assert_eq!(surfaced.adapter, "owner_adapter");
    assert_eq!(surfaced.value, value);
    assert_eq!(surfaced.canonical_message, None);
}

#[test]
fn terminal_surface_preserves_owner_canonical_message_exactly() {
    let canonical = "  owner-authored completion\n";
    let mut decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            AuthoritativeWaitOwnerResult {
                adapter: "owner_adapter".to_string(),
                value: serde_json::json!({"message": "different structured value"}),
                surfaceable_message: Some(canonical.to_string()),
            },
        )),
        ..Default::default()
    };

    let surfaced =
        authoritative_wait_terminal_surface(&decision, None, 10_000).expect("typed surface");
    assert_eq!(surfaced.canonical_message.as_deref(), Some(canonical));
    let schema = serde_json::json!({
        "type": "object", "properties": {"answer": {"type": "integer"}}, "required": ["answer"]
    });
    let schema = crate::session::turn_context::PreparedOutputSchema::prepare(schema).unwrap();
    assert!(authoritative_wait_terminal_surface(&decision, Some(&schema), 10_000).is_none());
    if let Some(AuthoritativeWaitResolution::Terminal(result)) = &mut decision.authoritative_wait {
        result.surfaceable_message = Some(r#"{"answer":42}"#.to_string());
    }
    assert!(authoritative_wait_terminal_surface(&decision, Some(&schema), 10_000).is_some());
    assert!(authoritative_wait_terminal_surface(&decision, Some(&schema), 0).is_none());
}

#[test]
fn projected_prompt_pressure_does_not_add_stable_tools_to_server_usage_twice() {
    assert_eq!(
        projected_prompt_tokens_from_estimates(
            /*active_context_tokens*/ 900, /*committed_history_tokens*/ 500,
            /*pending_token_estimate*/ 450, /*pending_body_growth_tokens*/ 50,
        ),
        950
    );
    assert_eq!(
        projected_prompt_tokens_from_estimates(
            /*active_context_tokens*/ 1_200, /*committed_history_tokens*/ 500,
            /*pending_token_estimate*/ 450, /*pending_body_growth_tokens*/ 0,
        ),
        1_200
    );
}

#[tokio::test]
async fn completed_measurements_calibrate_next_prompt_pressure_with_safe_fallbacks() {
    use codex_protocol::protocol::TokenUsage;

    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let pending = PendingTokenEstimate {
        total_tokens: 10_000,
        body_growth_tokens: 10,
        has_real_user_boundary: false,
    };
    assert_eq!(session.calibrated_prompt_tokens(&turn, 301).await, 301);
    // A pessimistic recent sample stays in force until four new samples replace it.
    for (local, measured, numerator, denominator) in [
        (200, 100, 1, 2),
        (50, 100, 2, 1),
        (200, 100, 2, 1),
        (200, 100, 2, 1),
        (200, 100, 2, 1),
        (200, 100, 1, 2),
    ] {
        turn.turn_timing_state = Arc::new(crate::turn_timing::TurnTimingState::default());
        let timing = &turn.turn_timing_state;
        timing.mark_turn_started();
        drop(timing.begin_model_request_wait());
        timing.record_model_attempt_identity("measurement", "physical");
        timing.record_model_request_payload(
            "measurement",
            "physical",
            serde_json::json!({"model": turn.model_info.slug})
                .to_string()
                .as_bytes(),
        );
        timing.record_model_request_sections(
            "measurement",
            "physical",
            serde_json::json!({"model": turn.model_info.slug})
                .to_string()
                .as_bytes(),
        );
        timing.record_model_request_token_categories(
            "measurement",
            "physical",
            codex_protocol::protocol::TurnTimingRequestTokenCategories {
                local_input_estimate: local,
                ..Default::default()
            },
        );
        let usage = TokenUsage {
            input_tokens: measured,
            total_tokens: measured,
            ..Default::default()
        };
        timing.record_generation_token_usage(Some(&usage));
        assert_eq!(
            timing.prompt_token_calibration_sample(&turn.model_info.slug, &usage),
            None
        );
        timing.record_response_event_milestones(&ResponseEvent::Completed {
            response_id: "response".to_string(),
            token_usage: Some(usage.clone()),
            end_turn: Some(true),
        });
        assert_eq!(
            timing.prompt_token_calibration_sample("different-model", &usage),
            None
        );
        session
            .record_token_usage_info(&turn, Some(&usage))
            .await
            .unwrap();
        assert_eq!(
            session.calibrated_prompt_tokens(&turn, 301).await,
            (301 * numerator + denominator - 1) / denominator
        );
        let local_prompt =
            session.get_estimated_token_count(&turn).await.unwrap() + pending.total_tokens;
        let floor = session.get_total_token_usage().await + pending.body_growth_tokens;
        let expected = ((local_prompt * numerator + denominator - 1) / denominator).max(floor);
        assert_eq!(
            projected_prompt_pressure(&session, &turn, pending)
                .await
                .total_tokens,
            expected
        );
    }
    let mut next = session
        .new_default_turn_with_sub_id("calibrated-next-turn".to_string())
        .await;
    assert_eq!(session.calibrated_prompt_tokens(&next, 301).await, 151);
    let next = Arc::get_mut(&mut next).expect("unique next-turn fixture");
    next.model_info.slug = "different-model".to_string();
    assert_eq!(session.calibrated_prompt_tokens(next, 301).await, 301);
    next.model_info.slug = turn.model_info.slug.clone();
    Arc::make_mut(&mut next.config).model_provider_id = "different-provider".to_string();
    assert_eq!(session.calibrated_prompt_tokens(next, 301).await, 301);

    // An accounting-less completion invalidates the old calibration; no billing
    // is invented and subsequent planning falls back to the local estimator.
    session.record_token_usage_info(&turn, None).await.unwrap();
    assert_eq!(session.calibrated_prompt_tokens(&turn, 301).await, 301);
    let usage = TokenUsage {
        input_tokens: 100,
        total_tokens: 100,
        ..Default::default()
    };
    turn.turn_timing_state
        .record_model_attempt_identity("measurement", "retry");
    assert_eq!(
        turn.turn_timing_state
            .prompt_token_calibration_sample(&turn.model_info.slug, &usage),
        None
    );
    session
        .record_token_usage_info(&turn, Some(&usage))
        .await
        .unwrap();
    assert_eq!(session.calibrated_prompt_tokens(&turn, 301).await, 301);
}

#[test]
fn projected_prompt_pressure_adds_pending_body_growth_to_server_usage() {
    assert_eq!(
        projected_prompt_tokens_from_estimates(
            /*active_context_tokens*/ 1_200, /*committed_history_tokens*/ 500,
            /*pending_token_estimate*/ 450, /*pending_body_growth_tokens*/ 50,
        ),
        1_250
    );
}

#[test]
fn assistant_stream_parsers_reuse_state_across_interleaved_deltas() {
    let mut parsers = AssistantMessageStreamParsers::new(/*plan_mode*/ true);
    let first = parsers.seed_item_text("one", "hello\n<proposed");
    assert_eq!(first.visible_text, "hello\n");
    assert_eq!(parsers.parse_delta("two", "other").visible_text, "other");
    let next = parsers.parse_delta("one", "_plan>\n- step\n</proposed_plan>\n world");
    assert_eq!(next.visible_text, " world");
    assert_eq!(
        next.plan_segments,
        vec![
            ProposedPlanSegment::ProposedPlanStart,
            ProposedPlanSegment::ProposedPlanDelta("- step\n".to_string()),
            ProposedPlanSegment::ProposedPlanEnd,
            ProposedPlanSegment::Normal(" world".to_string()),
        ]
    );
    assert_eq!(parsers.parsers_by_item.len(), 2);
    assert!(parsers.finish_item("one").is_empty());
    assert!(!parsers.parsers_by_item.contains_key("one"));
    assert_eq!(parsers.parse_delta("two", " tail").visible_text, " tail");
    assert!(parsers.finish_item("two").is_empty());
    assert!(parsers.parsers_by_item.is_empty());
    assert!(parsers.finish_item("absent").is_empty());
}

#[test]
fn pending_turn_mechanism_retries_remain_bounded_without_fixed_point_state() {
    let mut iterations = 0;
    for _ in 0..8 {
        advance_pending_turn_plan_iteration(&mut iterations, "inventory changed")
            .expect("within retry bound");
    }
    assert_eq!(
        advance_pending_turn_plan_iteration(&mut iterations, "inventory changed"),
        Err("pending-turn planning did not stabilize after 8 iterations; last retry: inventory changed".to_string())
    );
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the held guard is the contention this test asserts stabilization waits on"
)]
async fn pending_turn_exhausted_budget_stops_before_history_or_snapshot_work() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let mut client = session.services.model_client.new_session();
    let mut iterations = MAX_PENDING_TURN_PLAN_ITERATIONS;
    let mut completed_effect = None;
    // Any history inspection or step capture would wait on this state owner.
    let state = session.state.lock().await;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        stabilize_pending_turn_plan(
            &session,
            &turn_context,
            &[],
            &mut client,
            &mut iterations,
            &mut completed_effect,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("exhausted planning must stop before waiting for session state");
    drop(state);

    assert!(
        matches!(result, Err(CodexErr::Fatal(message)) if message.contains("did not stabilize after 8 iterations; last retry: retry budget was exhausted before this planning invocation"))
    );
    assert_eq!(completed_effect, None);
    assert_eq!(
        turn_context
            .turn_timing_state
            .complete_snapshot()
            .profile
            .counters
            .planning_failure_count,
        1
    );
    assert!(session.clone_history().await.raw_items().is_empty());
}

#[tokio::test]
async fn pending_turn_missing_inventory_records_a_planning_failure() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let mut client = session.services.model_client.new_session();
    let mut iterations = 0;
    let mut completed_effect = Some((
        "installed-probe".to_string(),
        Some(HashSet::from(["absent-inventory-key".to_string()])),
    ));
    let result = stabilize_pending_turn_plan(
        &session,
        &turn_context,
        &[],
        &mut client,
        &mut iterations,
        &mut completed_effect,
        &CancellationToken::new(),
    )
    .await;

    assert!(matches!(
        result,
        Err(CodexErr::Fatal(message)) if message.contains("installed-probe")
            && message.contains("missing its expected model-visible state")
    ));
    assert_eq!(
        turn_context
            .turn_timing_state
            .complete_snapshot()
            .profile
            .counters
            .planning_failure_count,
        1
    );
    assert!(session.clone_history().await.raw_items().is_empty());
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the held guard proves cancellation returns without waiting for the state owner"
)]
async fn pending_turn_cancelled_before_planning_does_not_charge_budget() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let mut client = session.services.model_client.new_session();
    let mut iterations = 0;
    let mut completed_effect = None;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let state = session.state.lock().await;

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        stabilize_pending_turn_plan(
            &session,
            &turn_context,
            &[],
            &mut client,
            &mut iterations,
            &mut completed_effect,
            &cancellation,
        ),
    )
    .await
    .expect("cancelled planning must not wait for session state");
    drop(state);

    assert!(matches!(result, Err(CodexErr::TurnAborted)));
    assert_eq!(iterations, 0);
    assert_eq!(completed_effect, None);
    assert!(session.clone_history().await.raw_items().is_empty());
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the held state owner proves legacy follow-up renewal precedes planning"
)]
async fn legacy_followup_renews_at_turn_entry_but_cancelled_turn_does_not() {
    use codex_agent_task_store::AcceptanceCriterion;
    use codex_agent_task_store::AgentRole;
    use codex_agent_task_store::AgentTaskBindingDraft;
    use codex_agent_task_store::AssignmentAdmissionOrigin;
    use codex_agent_task_store::AssignmentDraft;
    use codex_agent_task_store::CapabilityProfile;
    use codex_agent_task_store::WorkspaceStrategy;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;

    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .unwrap();
    let coordinator = session.services.agent_control.task_coordinator();
    coordinator
        .initialize(state, "root-session".to_string())
        .await
        .unwrap();
    let (assignment, attempt) = coordinator
        .create_assignment(
            repo.path(),
            AssignmentDraft {
                root_session_id: "root-session".to_string(),
                admission_origin: AssignmentAdmissionOrigin::LegacyMessage {
                    parent_assignment_id: None,
                },
                role: AgentRole::Worker,
                capability_profile: CapabilityProfile::ScopedSourceWrite,
                objective: "complete the follow-up".to_string(),
                acceptance_criteria: vec![AcceptanceCriterion {
                    evidence_policy: None,
                    id: "completed".to_string(),
                    text: "follow-up completed".to_string(),
                }],
                read_scope: Vec::new(),
                write_scope: Vec::new(),
                stop_condition: "task complete".to_string(),
                dependencies: Vec::new(),
                risk_hints: Vec::new(),
                required_evidence: Vec::new(),
                prohibited_changes: Vec::new(),
                contract_claims: Vec::new(),
                workspace_strategy: WorkspaceStrategy::Shared,
                relation: None,
                architecture_contract_ref: None,
            },
        )
        .await
        .unwrap();
    let path = AgentPath::root().join("followup_worker").unwrap();
    coordinator
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: path.to_string(),
            task_name: "followup_worker".to_string(),
            thread_id: Some(session.thread_id.to_string()),
        })
        .await
        .unwrap();
    turn_context.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: codex_protocol::ThreadId::new(),
        depth: 1,
        agent_path: Some(path.clone()),
        agent_nickname: None,
        agent_role: None,
    });
    coordinator
        .seal_missing_receipt(
            &path,
            session.thread_id,
            &AgentStatus::Completed(Some("initial result".to_string())),
        )
        .await
        .unwrap()
        .expect("initial completion seals its attempt");
    let mut stale_turn = turn_context
        .with_model(
            turn_context.model_info.slug.clone(),
            &session.services.models_manager,
        )
        .await;
    stale_turn.agent_task_binding = Arc::new(std::sync::OnceLock::from(
        coordinator.binding_for_agent_path(&path),
    ));
    stale_turn.multi_agent_version = MultiAgentVersion::V2;
    let turn_context = Arc::new(turn_context);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let mut budget = LogicalGenerationBudget::default();
    let result = run_turn(
        Arc::clone(&session),
        Arc::clone(&turn_context),
        Arc::new(ExtensionData::new(turn_context.sub_id.clone())),
        Vec::new(),
        None,
        &mut budget,
        cancelled,
    )
    .await;
    assert!(matches!(result, Err(CodexErr::TurnAborted)));
    assert_eq!(
        coordinator
            .binding_for_source(&turn_context.session_source)
            .unwrap()
            .attempt_id,
        attempt.attempt_id
    );
    assert_eq!(budget.regular_generations, 0);

    // Stop planning at its first state read. The attempt must already be fresh,
    // and the cancellation must propagate without charging a model generation.
    let state = session.state.lock().await;
    let cancellation = CancellationToken::new();
    let turn_cancellation = cancellation.clone();
    let running_session = Arc::clone(&session);
    let running_context = Arc::clone(&turn_context);
    let running = tokio::spawn(async move {
        let mut budget = LogicalGenerationBudget::default();
        let extension_data = Arc::new(ExtensionData::new(running_context.sub_id.clone()));
        let result = run_turn(
            running_session,
            running_context,
            extension_data,
            Vec::new(),
            None,
            &mut budget,
            turn_cancellation,
        )
        .await;
        (result, budget)
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if coordinator
                .binding_for_source(&turn_context.session_source)
                .unwrap()
                .attempt_id
                != attempt.attempt_id
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("run_turn must renew the completed binding before planning");
    cancellation.cancel();
    drop(state);
    let (result, budget) = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("cancelled follow-up returns without model work")
        .unwrap();
    assert!(matches!(result, Err(CodexErr::TurnAborted)));
    assert_eq!(budget.regular_generations, 0);
    let task = coordinator
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .unwrap();
    assert_eq!(task.current_attempt.ordinal, 1);
    assert!(task.receipt.is_none());
    assert!(
        session
            .maybe_notify_parent_of_terminal_turn(
                &stale_turn,
                &EventMsg::TurnComplete(codex_protocol::protocol::TurnCompleteEvent {
                    surfaced_result: None,
                    turn_id: stale_turn.sub_id.clone(),
                    last_agent_message: Some("late old result".to_string()),
                    error: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                    timing: None,
                })
            )
            .await
    );
    let after_stale = coordinator
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .unwrap();
    assert_eq!(
        after_stale.current_attempt.attempt_id,
        task.current_attempt.attempt_id
    );
    assert!(
        after_stale.receipt.is_none(),
        "a stale Session callback must not seal the renewed attempt"
    );
    assert!(session.clone_history().await.raw_items().is_empty());
}

#[test]
fn pending_turn_mechanism_does_not_replay_the_completed_mcp_effect() {
    let completed = ("install:tool@v1".to_string(), None);
    assert!(mcp_dependency_effect_is_completed(
        Some(&completed),
        "install:tool@v1"
    ));
    assert!(!mcp_dependency_effect_is_completed(
        Some(&completed),
        "install:tool@v2"
    ));
}

#[test]
fn pending_turn_mechanism_inventory_effect_requires_a_newer_generation() {
    for after_generation in [3, 4] {
        assert_eq!(
            require_newer_planning_generation("install:tool@v1", 4, after_generation),
            Err(format!(
                "inventory effect `install:tool@v1` completed at planning generation 4, but the next observable generation {after_generation} did not advance"
            ))
        );
    }
    assert!(require_newer_planning_generation("install:tool@v1", 4, 5).is_ok());
}

#[tokio::test]
async fn post_sampling_state_reads_start_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let pending_barrier = Arc::clone(&barrier);
    let token_barrier = Arc::clone(&barrier);

    let values = tokio::time::timeout(
        Duration::from_millis(200),
        collect_post_sampling_state(
            async move {
                pending_barrier.wait().await;
                "pending"
            },
            async move {
                token_barrier.wait().await;
                "tokens"
            },
        ),
    )
    .await
    .expect("independent post-sampling reads should both start without waiting for the other");

    assert_eq!(values, ("pending", "tokens"));
}

#[tokio::test]
async fn plugin_recommendation_and_mcp_tool_reads_start_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let recommendation_barrier = Arc::clone(&barrier);
    let mcp_barrier = Arc::clone(&barrier);

    let values = tokio::time::timeout(
        Duration::from_millis(200),
        join_recommendations_and_mcp(
            async move {
                recommendation_barrier.wait().await;
                "recommendations"
            },
            async move {
                mcp_barrier.wait().await;
                "mcp"
            },
        ),
    )
    .await
    .expect("independent recommendation and MCP reads should overlap");

    assert_eq!(values, ("recommendations", "mcp"));
}

#[tokio::test]
async fn projected_prompt_state_reads_start_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let active_barrier = Arc::clone(&barrier);
    let history_barrier = Arc::clone(&barrier);
    let compact_barrier = Arc::clone(&barrier);

    let values = tokio::time::timeout(
        Duration::from_millis(200),
        collect_projected_prompt_state(
            async move {
                active_barrier.wait().await;
                "active"
            },
            async move {
                history_barrier.wait().await;
                "history"
            },
            async move {
                compact_barrier.wait().await;
                "compact"
            },
        ),
    )
    .await
    .expect("independent prompt-pressure state reads should overlap");

    assert_eq!(values, ("active", "history", "compact"));
}

#[test]
fn planned_plugin_mention_emits_persisted_identity_and_turn_metadata() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "planned_plugin_mention_emits_persisted_identity_and_turn_metadata",
        planned_plugin_mention_emits_persisted_identity_and_turn_metadata_impl,
    )
}

async fn planned_plugin_mention_emits_persisted_identity_and_turn_metadata_impl() -> Result<()> {
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;
    use codex_config::ConfigLayerStack;
    use codex_config::ConfigRequirements;
    use codex_config::ConfigRequirementsToml;
    use codex_core_plugins::PluginsManager;
    use codex_core_plugins::store::PluginStore;
    use codex_plugin::PluginId;
    use codex_utils_absolute_path::AbsolutePathBuf;

    let home = tempfile::tempdir()?;
    let source = home.path().join("source");
    fs::create_dir_all(source.join(".codex-plugin"))?;
    fs::write(
        source.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample","version":"1.0.0","description":"Mentioned plugin"}"#,
    )?;
    fs::create_dir_all(source.join("skills/sample"))?;
    fs::write(
        source.join("skills/sample/SKILL.md"),
        "---\nname: sample\ndescription: A real plugin capability for mention discovery.\n---\nUse the sample capability.\n",
    )?;
    let plugin_id = PluginId::parse("sample@test")?;
    let store = PluginStore::new(home.path().to_path_buf());
    store.install(AbsolutePathBuf::try_from(source)?, plugin_id.clone())?;
    store.write_remote_plugin_id(&plugin_id, "plugins~persisted-sample")?;

    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/analytics-events/events"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let (mut session, mut turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let auth =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let session_mut = Arc::get_mut(&mut session).expect("unique session");
    session_mut.services.plugins_manager = Arc::new(PluginsManager::new(home.path().to_path_buf()));
    session_mut.services.auth_manager = Arc::clone(&auth);
    session_mut.services.analytics_events_client = codex_analytics::AnalyticsEventsClient::new(
        Arc::clone(&auth),
        server.uri(),
        Some(true),
        codex_http_client::HttpClientFactory::new(
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        ),
    );
    let turn = Arc::get_mut(&mut turn_context).expect("unique turn");
    turn.auth_manager = Some(auth);
    let config = Arc::make_mut(&mut turn.config);
    config.codex_home = AbsolutePathBuf::try_from(home.path())?;
    config.features.enable(Feature::Plugins)?;
    config.features.disable(Feature::RemotePlugin)?;
    config.features.disable(Feature::ToolSuggest)?;
    config.config_layer_stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: AbsolutePathBuf::try_from(home.path().join("config.toml"))?,
                profile: None,
            },
            toml::from_str(
                r#"[plugins."sample@test"]
enabled = true
"#,
            )?,
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )?
    .into();
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Mention {
            name: "sample".to_string(),
            path: "plugin://sample@test".to_string(),
        }],
        client_id: None,
    }];
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await
        .unwrap();
    let PendingTurnPlanBuild::Ready(plan) = build_pure_pending_turn_plan(
        &session,
        step_context,
        &input,
        session.services.planning_generation(),
        &CancellationToken::new(),
    )
    .await?
    else {
        panic!("installed plugin mention must produce a ready plan");
    };
    assert_eq!(plan.mentioned_plugins.len(), 1);
    assert_eq!(plan.mentioned_plugins[0].config_name, "sample@test");
    commit_pending_turn_plan_effects(&session, &turn_context, &plan).await;

    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for request in server.received_requests().await.unwrap_or_default() {
                let payload: serde_json::Value = serde_json::from_slice(&request.body)?;
                if let Some(event) = payload["events"].as_array().and_then(|events| {
                    events
                        .iter()
                        .find(|event| event["event_type"] == "codex_plugin_used")
                }) {
                    return Ok::<_, anyhow::Error>(event.clone());
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let metadata = &event["event_params"];
    assert_eq!(metadata["plugin_id"], "sample@test");
    assert_eq!(metadata["remote_plugin_id"], "plugins~persisted-sample");
    assert_eq!(metadata["plugin_name"], "sample");
    assert_eq!(metadata["marketplace_name"], "test");
    assert_eq!(metadata["has_skills"], true);
    assert_eq!(metadata["mcp_server_names"], serde_json::json!([]));
    assert_eq!(metadata["thread_id"], session.thread_id.to_string());
    assert_eq!(metadata["turn_id"], turn_context.sub_id);
    assert_eq!(metadata["model_slug"], turn_context.model_info.slug);
    Ok(())
}

#[test]
fn plan_prose_prefix_survives_worker_abort_during_item_start() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "plan_prose_prefix_survives_worker_abort_during_item_start",
        plan_prose_prefix_survives_worker_abort_during_item_start_impl,
    )
}

async fn plan_prose_prefix_survives_worker_abort_during_item_start_impl() -> Result<()> {
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::Settings;
    use codex_protocol::protocol::RolloutItem;
    use codex_utils_absolute_path::AbsolutePathBuf;
    let server = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![responses::sse(vec![
            responses::ev_response_created("plan-prefix-abort"),
            responses::ev_message_item_added("plan-prefix-message", ""),
            responses::ev_output_text_delta("  \n"),
            responses::ev_output_text_delta("Accepted prose prefix.\n"),
            responses::ev_completed("plan-prefix-abort"),
        ])],
    )
    .await;
    let home = tempfile::tempdir()?;
    let mut config = crate::config::test_config().await;
    config.codex_home = AbsolutePathBuf::from_absolute_path(home.path())?;
    config.cwd = config.codex_home.clone();
    config.model_provider = non_openai_model_provider(&server);
    config.model_provider.request_max_retries = Some(0);
    config.model_provider.stream_max_retries = Some(0);
    let model = config.model.clone().expect("fixture model");
    let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        home.path().to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    // Compare the persisted canonical completion with the live completion.
    let started = manager
        .start_thread_with_options(manager.start_thread_options(config))
        .await?;
    let thread = started.thread;
    let session = Arc::clone(&thread.codex.session);
    let (reached, reached_rx) = async_channel::bounded(1);
    let (release, release_rx) = tokio::sync::oneshot::channel();
    PLAN_START_PUBLICATION_GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(session.thread_id, (reached, release_rx));
    let turn_id = thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Draft a plan.".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Plan,
                    settings: Settings {
                        model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(15), reached_rx.recv()).await??;
    let (turn, worker_abort, worker_done) = {
        let active = session.active_turn.lock().await;
        let task = active
            .as_ref()
            .and_then(|active| active.task.as_ref())
            .expect("actual sampling worker");
        (
            Arc::clone(&task.turn_context),
            task.worker_abort_handle.clone(),
            Arc::clone(&task.worker_done),
        )
    };
    assert_eq!(turn.sub_id, turn_id);
    worker_abort.abort();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !worker_done.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original sampling worker actually dropped");
    // Interrupt uses the normal terminal owner; its ordered barrier must wait for the accepted prefix.
    thread.submit(Op::Interrupt).await?;
    release
        .send(())
        .expect("accepted publication remains owned after caller abort");
    tokio::time::timeout(
        Duration::from_secs(15),
        session.flush_rollout_after_ordered_commits(&turn),
    )
    .await??;
    thread.submit(Op::Shutdown).await?;
    let mut live = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = thread.next_event().await.expect("normal event stream").msg;
            let done = matches!(event, EventMsg::ShutdownComplete);
            live.push(event);
            if done {
                break;
            }
        }
    })
    .await
    .expect("normal shutdown flushes partial publication");
    let history = crate::rollout::recorder::RolloutRecorder::get_rollout_history(
        &thread.rollout_path().expect("physical rollout"),
    )
    .await?;
    let physical = history
        .get_rollout_items()
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(event) => Some(event.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = "  \nAccepted prose prefix.\n";
    for (name, events) in [("live", &live), ("physical", &physical)] {
        let completed = events
            .iter()
            .filter_map(|event| match event {
                EventMsg::ItemCompleted(event) => match &event.item {
                    TurnItem::AgentMessage(item) if item.id == "plan-prefix-message" => {
                        Some(agent_message_text(item))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            completed,
            vec![expected],
            "{name}: accepted prefix is completed exactly once"
        );
        let starts = events.iter().filter(|event| matches!(event,
            EventMsg::ItemStarted(event) if matches!(&event.item, TurnItem::AgentMessage(item) if item.id == "plan-prefix-message"))).count();
        assert_eq!(
            starts,
            if name == "live" { 1 } else { 0 },
            "deferred start is delivered once and remains transient in physical history"
        );
    }
    let deltas = live
        .iter()
        .filter_map(|event| match event {
            EventMsg::AgentMessageContentDelta(event) if event.item_id == "plan-prefix-message" => {
                Some(event.delta.as_str())
            }
            _ => None,
        })
        .collect::<String>();
    assert_eq!(
        deltas, expected,
        "live prefix includes the buffered leading whitespace exactly once"
    );
    assert_eq!(
        requests.requests().len(),
        1,
        "the real plan stream was not retried"
    );
    Ok(())
}

#[test]
fn audit_stop_hook_unchanged_failure_stops_after_one_repair() -> Result<()> {
    run_turn_multi_thread_test_with_stack("audit_stop_hook_unchanged_failure", || async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let requests = responses::mount_sse_sequence(
            &server,
            (0..2)
                .map(|index| {
                    responses::sse(vec![
                        responses::ev_assistant_message(&format!("draft-{index}"), "draft answer"),
                        responses::ev_completed(&format!("response-{index}")),
                    ])
                })
                .collect(),
        )
        .await;
        let test = test_codex().with_pre_build_hook(|home| {
            let script = home.join("always_block.py");
            fs::write(&script, "import json,sys\njson.load(sys.stdin)\nprint(json.dumps({'decision':'block','reason':'same required validation'}))\n").unwrap();
            fs::write(home.join("hooks.json"), serde_json::json!({"hooks":{"Stop":[{"hooks":[{
                "type":"command", "command":format!("python3 \"{}\"", script.display()),
                "commandWindows":format!("python \"{}\"", script.display())
            }]}]}}).to_string()).unwrap();
        }).with_config(trust_discovered_hooks).build(&server).await?;
        let completion = test
            .submit_turn_and_capture_completion("Finish the answer")
            .await?;
        assert!(completion.error.is_some(), "{completion:?}");
        assert_eq!(requests.requests().len(), 2);
        Ok(())
    })
}

#[test]
fn completion_boundary_hook_repairs_compare_the_rejected_answer() -> Result<()> {
    run_turn_multi_thread_test_with_stack("completion_boundary_hook_repairs", || async {
        crate::test_harness::require_network!();
        for unrelated_read in [false, true] {
            let server = responses::start_mock_server().await;
            let answer = |id: &str, text: &str| {
                responses::sse(vec![
                    responses::ev_assistant_message(id, text),
                    responses::ev_completed(id),
                ])
            };
            let middle = if unrelated_read {
                responses::sse(vec![
                    responses::ev_function_call(
                        "unrelated",
                        "read_file",
                        r#"{"path":"unrelated.txt"}"#,
                    ),
                    responses::ev_completed("read"),
                ])
            } else {
                answer("changed", "changed draft")
            };
            let requests = responses::mount_sse_sequence(
                &server,
                vec![
                    answer("first", "draft"),
                    middle,
                    answer(
                        "repeat",
                        if unrelated_read {
                            "draft"
                        } else {
                            "changed draft"
                        },
                    ),
                ],
            )
            .await;
            let test = test_codex().with_pre_build_hook(|home| {
                let script = home.join("block_completion.py");
                fs::write(&script, "import json,sys\njson.load(sys.stdin)\nprint(json.dumps({'decision':'block','reason':'same required repair'}))\n").unwrap();
                fs::write(home.join("hooks.json"), serde_json::json!({"hooks":{"Stop":[{"hooks":[{
                    "type":"command", "command":format!("python3 \"{}\"", script.display()),
                    "commandWindows":format!("python \"{}\"", script.display())
                }]}]}}).to_string()).unwrap();
            }).with_config(trust_discovered_hooks).build(&server).await?;
            fs::write(test.workspace_path("unrelated.txt"), "unrelated evidence")?;
            let completion = test
                .submit_turn_and_capture_completion("Finish the answer")
                .await?;
            assert!(completion.error.is_some(), "{completion:?}");
            assert_eq!(
                requests.requests().len(),
                3,
                "unrelated_read={unrelated_read}"
            );
        }
        Ok(())
    })
}

#[test]
fn completion_boundary_rejected_direct_answers_are_never_published() -> Result<()> {
    run_turn_multi_thread_test_with_stack("completion_boundary_direct_hooks", || async {
        crate::test_harness::require_network!();
        for mode in ["repair-model", "repair-direct", "abort"] {
            let server = responses::start_mock_server().await;
            let direct = |id: &str, text: &str| {
                responses::sse(vec![
                    responses::ev_custom_tool_call(
                        id,
                        "exec",
                        &format!("// @exec: {{\"deliver\":true}}\ntext('{text}');"),
                    ),
                    responses::ev_completed(id),
                ])
            };
            let mut sequence = vec![direct("candidate-a", "rejected A")];
            if mode == "repair-direct" {
                sequence.push(direct("candidate-b", "accepted B"));
            }
            if mode == "repair-model" {
                sequence.push(responses::sse(vec![
                    responses::ev_assistant_message("candidate-b", "accepted B"),
                    responses::ev_completed("accepted"),
                ]));
            }
            let requests = responses::mount_sse_sequence(&server, sequence).await;
            let test = test_codex()
                .with_pre_build_hook(move |home| {
                    if mode != "abort" {
                        write_one_shot_stop_hook(home).unwrap();
                    }
                })
                .with_config(move |config| {
                    if mode != "abort" {
                        trust_discovered_hooks(config);
                    }
                    config.features.enable(Feature::CodeMode).unwrap();
                    config.features.enable(Feature::Kd4Runtime).unwrap();
                    if mode == "abort" {
                        config.after_agent_policy = AfterAgentPolicy::MutatingFinalizer;
                        config.notify = Some(vec![
                            if cfg!(windows) { "python" } else { "python3" }.into(),
                            "-c".into(),
                            "import sys; sys.exit(1)".into(),
                        ]);
                    }
                })
                .build(&server)
                .await?;
            let completion = test
                .submit_turn_and_capture_completion("Finish the answer")
                .await?;
            assert_eq!(completion.error.is_some(), mode == "abort");
            assert_eq!(
                completion.surfaced_result.is_some(),
                mode == "repair-direct"
            );
            if mode != "abort" {
                assert_eq!(completion.last_agent_message.as_deref(), Some("accepted B"));
            }
            test.codex.flush_rollout().await?;
            let (items, _, errors) = crate::rollout::recorder::RolloutRecorder::load_rollout_items(
                &test.codex.rollout_path().unwrap(),
            )
            .await?;
            assert_eq!(errors, 0);
            let answers = items
                .iter()
                .filter_map(|item| match item {
                    codex_protocol::protocol::RolloutItem::ResponseItem(
                        ResponseItem::Message { role, content, .. },
                    ) if role == "assistant" => Some(content),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if mode == "abort" {
                assert!(answers.is_empty());
            } else {
                assert_eq!(
                    answers,
                    vec![&vec![ContentItem::OutputText {
                        text: "accepted B".into()
                    }]]
                );
            }
            assert_eq!(
                requests.requests().len(),
                if mode == "abort" { 1 } else { 2 }
            );
        }
        Ok(())
    })
}

#[test]
fn audit_stop_hook_alternating_feedback_stops_when_a_repair_repeats() -> Result<()> {
    run_turn_multi_thread_test_with_stack("audit_stop_hook_alternating_feedback", || async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let requests = responses::mount_sse_sequence(
            &server,
            (0..3)
                .map(|index| {
                    responses::sse(vec![
                        responses::ev_assistant_message(&format!("draft-{index}"), "draft answer"),
                        responses::ev_completed(&format!("response-{index}")),
                    ])
                })
                .collect(),
        )
        .await;
        let test = test_codex().with_pre_build_hook(|home| {
            let script = home.join("alternate_block.py");
            fs::write(&script, "import json,sys,pathlib\njson.load(sys.stdin)\np=pathlib.Path(__file__).with_suffix('.count')\nn=int(p.read_text()) if p.exists() else 0\np.write_text(str(n+1))\nprint(json.dumps({'decision':'block','reason':['required A','required B'][n%2]}))\n").unwrap();
            fs::write(home.join("hooks.json"), serde_json::json!({"hooks":{"Stop":[{"hooks":[{
                "type":"command", "command":format!("python3 \"{}\"", script.display()),
                "commandWindows":format!("python \"{}\"", script.display())
            }]}]}}).to_string()).unwrap();
        }).with_config(trust_discovered_hooks).build(&server).await?;
        let completion = test
            .submit_turn_and_capture_completion("Finish the answer")
            .await?;
        assert!(completion.error.is_some(), "{completion:?}");
        assert_eq!(requests.requests().len(), 3);
        Ok(())
    })
}

#[test]
fn source_changing_finalizer_reports_unverified_state_without_validation_replay() -> Result<()> {
    run_turn_multi_thread_test_with_stack("post_validation_finalizer_boundary", || async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let python = if cfg!(windows) { "python" } else { "python3" };
        let command = serde_json::json!({
            "program": python, "args": ["-B", "-m", "unittest", "test_fixture"],
            "yield_time_ms": 300000,
        })
        .to_string();
        let requests = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_function_call("validate", "exec_command", &command),
                    responses::ev_completed("validated"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("answer", "The check passed."),
                    responses::ev_completed("done"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "limitation",
                        "The finalizer changed the workspace; the final state is unverified.",
                    ),
                    responses::ev_completed("final"),
                ]),
            ],
        )
        .await;
        let test = test_codex().with_config(move |config| {
            config.features.enable(Feature::Kd4Runtime).unwrap();
            config.features.enable(Feature::UnifiedExec).unwrap();
            config.features.disable(Feature::CodeModeHost).unwrap();
            config.after_agent_policy = AfterAgentPolicy::MutatingFinalizer;
            config.notify = Some(vec![python.into(), "-c".into(),
                "import pathlib,sys; p=pathlib.Path(sys.argv[1]); p.write_text('changed'); c=p.with_suffix('.count'); c.write_text(c.read_text()+'1' if c.exists() else '1')".into(),
                config.cwd.join("source.txt").to_string_lossy().into_owned(),
            ]);
        }).build(&server).await?;
        fs::write(test.workspace_path("source.txt"), "original")?;
        fs::write(
            test.workspace_path("test_fixture.py"),
            "import pathlib,unittest\nclass Check(unittest.TestCase):\n def test_source(self):\n  self.assertEqual(pathlib.Path('source.txt').read_text(), 'original')\n",
        )?;
        // Uncertain commands require a repository baseline to prove that
        // validation did not mutate the finalizer's workspace.
        let initialized = tokio::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(test.workspace_path("."))
            .status()
            .await?;
        assert!(initialized.success());
        test.codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "Run the check and finish.".into(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                // This test exercises a mutating finalizer without automatic revalidation.
                // Permit the local test command explicitly on hosts without a sandbox
                // backend; sandbox refusal has its own focused coverage.
                thread_settings: ThreadSettingsOverrides {
                    permission_profile: Some(PermissionProfile::Disabled),
                    ..Default::default()
                },
            })
            .await?;
        let mut boundary_reported = false;
        let mut observed_events = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = test.codex.next_event().await.unwrap().msg;
                observed_events.push(format!("{event:?}"));
                match event {
                    EventMsg::ExecApprovalRequest(approval) => {
                        test.codex.submit(Op::ExecApproval {
                            id: approval.approval_id.unwrap_or(approval.call_id),
                            turn_id: Some(approval.turn_id),
                            decision: codex_protocol::protocol::ReviewDecision::Approved,
                        }).await.unwrap();
                    }
                    EventMsg::Warning(warning) => boundary_reported |= warning.message.contains("validation check(s) failed"),
                    EventMsg::TurnComplete(completion) => {
                        assert!(completion.error.is_none(), "{completion:?}");
                        assert_eq!(completion.last_agent_message.as_deref(), Some("The finalizer changed the workspace; the final state is unverified."));
                        let assessment = completion.timing.unwrap().completion_assessment.unwrap();
                        assert!(assessment.failed_checks.is_empty());
                        assert!(!assessment.verification_gaps.is_empty());
                        assert!(assessment.validation_uncertainty.as_ref()
                            .is_none_or(|uncertainty| uncertainty.failed_checks.is_empty()));
                        break;
                    }
                    EventMsg::TurnAborted(event) => panic!("unexpected abort: {event:?}"),
                    _ => {}
                }
            }
        }).await.unwrap_or_else(|error| panic!("validation and finalizer complete: {error}; requests={}; events={observed_events:?}", requests.requests().len()));
        assert!(!boundary_reported, "failed checks no longer synthesize completion warnings");
        assert_eq!(
            fs::read_to_string(test.workspace_path("source.txt"))?,
            "changed"
        );
        assert_eq!(
            fs::read_to_string(test.workspace_path("source.count"))?,
            "1"
        );
        let sent = requests.requests();
        assert_eq!(sent.len(), 3);
        assert!(
            sent[1]
                .function_call_output_text("validate")
                .unwrap()
                .contains("OK")
        );
        assert!(sent[2].body_json()["tools"].as_array().unwrap().is_empty());
        assert!(sent.iter().all(|request| request.function_call_output_text("revalidate").is_none()));
        Ok(())
    })
}

#[test]
fn audit115_observed_preexisting_failure_is_reported_without_repair() -> Result<()> {
    run_turn_multi_thread_test_with_stack("audit115_observed_preexisting_failure", || {
        audit115_failing_validation_turn(/*changes_sources*/ false)
    })
}

#[test]
fn validation_failure_after_source_change_does_not_reopen_the_model() -> Result<()> {
    run_turn_multi_thread_test_with_stack("audit115_failure_after_source_change", || {
        audit115_failing_validation_turn(/*changes_sources*/ true)
    })
}

/// One turn runs a check that already fails before the turn starts. Observing
/// and reporting it is the result; after this turn's own source change the
/// same failure remains reportable without reopening the model.
async fn audit115_failing_validation_turn(changes_sources: bool) -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let python = if cfg!(windows) { "python" } else { "python3" };
    let command = serde_json::json!({
        "program": python, "args": ["-B", "-m", "unittest", "test_fixture"],
        "yield_time_ms": 300000,
    })
    .to_string();
    let report = "The fixture check fails with a pre-existing failure.";
    let mut sequence = Vec::new();
    if changes_sources {
        sequence.push(responses::sse(vec![
            responses::ev_apply_patch_custom_tool_call(
                "edit",
                "*** Begin Patch\n*** Add File: notes.txt\n+changed by this turn\n*** End Patch",
            ),
            responses::ev_completed("edited"),
        ]));
    }
    sequence.push(responses::sse(vec![
        responses::ev_function_call("validate", "exec_command", &command),
        responses::ev_completed("validated"),
    ]));
    // A final report ends either case; failures stay in the assessment.
    let reports = 1;
    for attempt in 0..reports {
        sequence.push(responses::sse(vec![
            responses::ev_assistant_message(&format!("report-{attempt}"), report),
            responses::ev_completed(&format!("reported-{attempt}")),
        ]));
    }
    let requests = responses::mount_sse_sequence(&server, sequence).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Kd4Runtime).unwrap();
            config.features.enable(Feature::UnifiedExec).unwrap();
            config.features.disable(Feature::CodeModeHost).unwrap();
        })
        .build(&server)
        .await?;
    fs::write(
        test.workspace_path("test_fixture.py"),
        "import unittest\nclass Check(unittest.TestCase):\n def test_source(self):\n  self.fail('pre-existing failure')\n",
    )?;
    // A repository baseline proves the uncertain check changed no source.
    let initialized = tokio::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(test.workspace_path("."))
        .status()
        .await?;
    assert!(initialized.success());
    let completion = tokio::time::timeout(
        Duration::from_secs(60),
        test.submit_turn_and_capture_completion("Run the fixture check and report its result."),
    )
    .await??;
    let sent = requests.requests();
    let output = sent
        .iter()
        .find_map(|request| request.function_call_output_text("validate"))
        .context("the model receives the validation result")?;
    assert!(output.contains("FAILED"), "{output}");
    assert_eq!(completion.last_agent_message.as_deref(), Some(report));
    let repairs = sent
        .iter()
        .map(|request| {
            request
                .message_input_texts("developer")
                .into_iter()
                .filter(|text| text.contains("Known current work prevents final acceptance"))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert!(completion.error.is_none(), "{completion:?}");
    assert_eq!(sent.len(), if changes_sources { 3 } else { 2 });
    assert!(repairs.iter().all(Vec::is_empty), "{repairs:?}");
    let assessment = completion.timing.as_ref()
        .and_then(|timing| timing.completion_assessment.as_ref())
        .context("completion assessment")?;
    assert_eq!(assessment.validation_uncertainty.as_ref().unwrap().failed_checks.len(), 1);
    Ok(())
}

#[test]
fn non_git_validation_rerun_after_fix_ends_without_repair() -> Result<()> {
    run_turn_multi_thread_test_with_stack("non_git_validation_rerun_after_fix", || {
        non_git_validation_rerun_turn(/*rerun_writes_tree*/ false)
    })
}

#[test]
fn non_git_validation_rerun_that_writes_the_tree_reports_stale_evidence() -> Result<()> {
    run_turn_multi_thread_test_with_stack("non_git_validation_rerun_writes_tree", || {
        non_git_validation_rerun_turn(/*rerun_writes_tree*/ true)
    })
}

/// Outside any Git repository, a failing check, this turn's fix and a passing
/// rerun that left the bounded workspace tree unchanged end without a repair.
/// A rerun that writes into the tree does not prove the final sources, so the
/// earlier failure remains reported, without an automatic repair.
async fn non_git_validation_rerun_turn(rerun_writes_tree: bool) -> Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let python = if cfg!(windows) { "python" } else { "python3" };
    let command = serde_json::json!({
        "program": python, "args": ["-B", "-m", "unittest", "test_fixture"],
        "yield_time_ms": 300000,
    })
    .to_string();
    let report = "The fixture check passes after the fix.";
    let mut sequence = vec![
        responses::sse(vec![
            responses::ev_function_call("validate", "exec_command", &command),
            responses::ev_completed("validated"),
        ]),
        responses::sse(vec![
            responses::ev_apply_patch_custom_tool_call(
                "fix",
                "*** Begin Patch\n*** Update File: lib.py\n@@\n-VALUE = 1\n+VALUE = 2\n*** End Patch",
            ),
            responses::ev_completed("fixed"),
        ]),
        responses::sse(vec![
            responses::ev_function_call("revalidate", "exec_command", &command),
            responses::ev_completed("revalidated"),
        ]),
    ];
    // Result freshness affects evidence, not the number of model turns.
    let reports = 1;
    for attempt in 0..reports {
        sequence.push(responses::sse(vec![
            responses::ev_assistant_message(&format!("report-{attempt}"), report),
            responses::ev_completed(&format!("reported-{attempt}")),
        ]));
    }
    let requests = responses::mount_sse_sequence(&server, sequence).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Kd4Runtime).unwrap();
            config.features.enable(Feature::UnifiedExec).unwrap();
            config.features.disable(Feature::CodeModeHost).unwrap();
        })
        .build(&server)
        .await?;
    // Unlike the audit 115 fixtures there is no `git init`, so neither check
    // has a Git-visible tree to compare.
    let outside_git = test
        .cwd_path()
        .ancestors()
        .all(|directory| !directory.join(".git").exists());
    assert!(
        outside_git,
        "the fixture workspace must be outside any Git repository"
    );
    fs::write(test.workspace_path("lib.py"), "VALUE = 1\n")?;
    // Only the guard's passing run writes a workspace file.
    let write = if rerun_writes_tree {
        "  if lib.VALUE == 2: pathlib.Path('generated.py').write_text('GENERATED = True')\n"
    } else {
        ""
    };
    fs::write(
        test.workspace_path("test_fixture.py"),
        format!(
            "import pathlib,unittest\nimport lib\nclass Check(unittest.TestCase):\n def test_value(self):\n{write}  self.assertEqual(lib.VALUE, 2)\n"
        ),
    )?;
    let completion = tokio::time::timeout(
        Duration::from_secs(60),
        test.submit_turn_and_capture_completion("Run the check, fix it and rerun it."),
    )
    .await??;
    let sent = requests.requests();
    let failed = sent
        .iter()
        .find_map(|request| request.function_call_output_text("validate"))
        .context("the model receives the failing result")?;
    assert!(failed.contains("FAILED"), "{failed}");
    let passed = sent
        .iter()
        .find_map(|request| request.function_call_output_text("revalidate"))
        .context("the model receives the rerun result")?;
    assert!(
        passed.contains("OK") && !passed.contains("FAILED"),
        "{passed}"
    );
    assert_eq!(completion.last_agent_message.as_deref(), Some(report));
    let repairs = sent
        .iter()
        .map(|request| {
            request
                .message_input_texts("developer")
                .into_iter()
                .filter(|text| text.contains("Known current work prevents final acceptance"))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert!(completion.error.is_none(), "{completion:?}");
    assert_eq!(sent.len(), 4);
    assert!(repairs.iter().all(Vec::is_empty), "{repairs:?}");
    if rerun_writes_tree {
        assert!(test.workspace_path("generated.py").exists());
        let assessment = completion.timing.as_ref()
            .and_then(|timing| timing.completion_assessment.as_ref())
            .context("stale passing run remains unverified")?;
        assert!(!assessment.verification_gaps.is_empty()
            || assessment.validation_uncertainty.as_ref().is_some_and(|uncertainty|
                !uncertainty.failed_checks.is_empty()));
    }
    Ok(())
}

#[test]
fn audit_finalizer_failure_precedes_stop_repair() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "audit_finalizer_failure_precedes_stop_repair",
        || async {
            crate::test_harness::require_network!();
            let server = responses::start_mock_server().await;
            let requests = responses::mount_sse_sequence(
                &server,
                vec![responses::sse(vec![
                    responses::ev_assistant_message("answer", "draft answer"),
                    responses::ev_completed("r1"),
                ])],
            )
            .await;
            let test = test_codex()
                .with_pre_build_hook(|home| {
                    write_one_shot_stop_hook(home).unwrap();
                })
                .with_config(|config| {
                    trust_discovered_hooks(config);
                    config.after_agent_policy = AfterAgentPolicy::MutatingFinalizer;
                    config.notify = Some(vec![
                        if cfg!(windows) { "python" } else { "python3" }.into(),
                        "-c".into(),
                        "import sys; sys.exit(1)".into(),
                    ]);
                })
                .build(&server)
                .await?;
            let completion = test
                .submit_turn_and_capture_completion("Finish the answer")
                .await?;
            assert!(completion.error.is_some(), "{completion:?}");
            assert_eq!(requests.requests().len(), 1);
            Ok(())
        },
    )
}

#[test]
fn audit_provider_overflow_recovers_once_in_the_same_turn() -> Result<()> {
    run_turn_multi_thread_test_with_stack("audit_provider_overflow_recovers_once", || async {
        crate::test_harness::require_network!();
        for repeated_overflow in [false, true] {
            let server = responses::start_mock_server().await;
            let overflow = responses::sse_failed(
                "overflow",
                "context_length_exceeded",
                "context window exceeded",
            );
            let final_response = if repeated_overflow {
                overflow.clone()
            } else {
                responses::sse(vec![
                    responses::ev_assistant_message("answer", "recovered answer"),
                    responses::ev_completed("done"),
                ])
            };
            let requests = responses::mount_sse_sequence(
                &server,
                vec![
                    overflow,
                    responses::sse(vec![
                        responses::ev_assistant_message(
                            "summary",
                            &complete_compaction_summary("continue the answer"),
                        ),
                        responses::ev_completed("compact"),
                    ]),
                    final_response,
                ],
            )
            .await;
            let provider = non_openai_model_provider(&server);
            let test = test_codex()
                .with_config(move |config| {
                    config.model_provider = provider;
                    config.model_provider.stream_max_retries = Some(0);
                    config.model_auto_compact_token_limit = Some(100_000);
                    config.model_context_window = Some(200_000);
                })
                .build(&server)
                .await?;
            let completion = test
                .submit_turn_and_capture_completion("Answer the user")
                .await?;
            assert_eq!(
                completion.error.is_some(),
                repeated_overflow,
                "{completion:?}"
            );
            if !repeated_overflow {
                assert_eq!(
                    completion.last_agent_message.as_deref(),
                    Some("recovered answer")
                );
            }
            assert_eq!(requests.requests().len(), 3);
        }
        Ok(())
    })
}

/// Sampling admission refuses a projected prompt before dispatch; the provider
/// never measured it. Each crossing is relieved like a limit found after
/// sampling instead of spending the turn's one provider-overflow recovery, so
/// a second crossing in a long turn compacts again rather than failing it.
#[test]
fn admission_refusal_is_relieved_at_each_crossing_in_the_same_turn() -> Result<()> {
    run_turn_multi_thread_test_with_stack("admission_refusal_relief", || async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        // Usage reports almost nothing after sampling. Only admission's
        // projection of the next prompt charges the long commentary, which
        // compaction does not retain, so the next prompt crosses the limit.
        let crossing = |id: &str, fill: &str| {
            responses::sse(vec![
                responses::ev_assistant_message(&format!("{id}-progress"), &fill.repeat(400_000)),
                responses::ev_function_call(&format!("{id}-call"), "unknown_test_tool", "{}"),
                responses::ev_completed_with_tokens(id, 1_000),
            ])
        };
        let summary = |id: &str| {
            responses::sse(vec![
                responses::ev_assistant_message(id, &complete_compaction_summary("continue")),
                responses::ev_completed_with_tokens(id, 1_000),
            ])
        };
        let requests = responses::mount_sse_sequence(
            &server,
            vec![
                crossing("first", "a"),
                summary("first-compaction"),
                crossing("second", "b"),
                summary("second-compaction"),
                responses::sse(vec![
                    responses::ev_assistant_message("answer", "recovered answer"),
                    responses::ev_completed_with_tokens("done", 1_000),
                ]),
            ],
        )
        .await;
        let provider = non_openai_model_provider(&server);
        let test = test_codex()
            .with_config(move |config| {
                config.model_provider = provider;
                config.model_provider.stream_max_retries = Some(0);
                config.model_auto_compact_token_limit = Some(60_000);
                config.model_context_window = Some(400_000);
            })
            .build(&server)
            .await?;
        let completion = test
            .submit_turn_and_capture_completion("Run the task")
            .await?;
        assert!(completion.error.is_none(), "{completion:?}");
        assert_eq!(
            completion.last_agent_message.as_deref(),
            Some("recovered answer")
        );
        assert_eq!(requests.requests().len(), 5);
        Ok(())
    })
}

/// A summary leaves no smaller history to summarize. When the summarized
/// prompt still overflows before any request completes, the turn ends rather
/// than spending another summary on the same history.
#[test]
fn overflow_after_an_unsampled_summary_ends_the_turn() -> Result<()> {
    run_turn_multi_thread_test_with_stack("overflow_after_unsampled_summary", || async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        responses::mount_sse_sequence(
            &server,
            vec![
                // Admission refuses the follow-up, and a summary relieves it.
                responses::sse(vec![
                    responses::ev_assistant_message("progress", &"a".repeat(400_000)),
                    responses::ev_function_call("call", "unknown_test_tool", "{}"),
                    responses::ev_completed_with_tokens("first", 1_000),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "summary",
                        &complete_compaction_summary("continue"),
                    ),
                    responses::ev_completed_with_tokens("compaction", 1_000),
                ]),
                // The provider still rejects the summarized prompt.
                responses::sse_failed(
                    "overflow",
                    "context_length_exceeded",
                    "context window exceeded",
                ),
            ],
        )
        .await;
        let provider = non_openai_model_provider(&server);
        let test = test_codex()
            .with_config(move |config| {
                config.model_provider = provider;
                config.model_provider.stream_max_retries = Some(0);
                config.model_auto_compact_token_limit = Some(60_000);
                config.model_context_window = Some(400_000);
            })
            .build(&server)
            .await?;
        let completion = test
            .submit_turn_and_capture_completion("Run the task")
            .await?;
        assert_eq!(
            completion.error.as_ref().map(|error| error.message.as_str()),
            Some(CodexErr::ContextWindowExceeded.to_string().as_str()),
            "{completion:?}"
        );
        let model_requests = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count();
        assert_eq!(model_requests, 3, "the summarized history was summarized again");
        Ok(())
    })
}

#[test]
fn oversized_required_instructions_stop_after_bounded_compaction() -> Result<()> {
    run_turn_multi_thread_test_with_stack("oversized_required_instructions", || async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let requests = responses::mount_sse_sequence(
            &server,
            vec![responses::sse(vec![
                responses::ev_assistant_message(
                    "summary",
                    &complete_compaction_summary("continue"),
                ),
                responses::ev_completed("compact"),
            ])],
        )
        .await;
        let provider = non_openai_model_provider(&server);
        let test = test_codex()
            .with_pre_build_hook(|home| {
                std::fs::write(
                    home.join("AGENTS.md"),
                    "Required safety and execution rule. ".repeat(4000),
                )
                .unwrap();
            })
            .with_config(move |config| {
                config.model_provider = provider;
                config.model_provider.stream_max_retries = Some(0);
                config.project_doc_max_bytes = 200_000;
                config.model_auto_compact_token_limit = Some(4_000);
                config.model_context_window = Some(8_000);
            })
            .build(&server)
            .await?;
        let completion = tokio::time::timeout(
            Duration::from_secs(20),
            test.submit_turn_and_capture_completion("Apply the required rules"),
        )
        .await??;
        assert!(
            completion.error.is_some(),
            "oversized mandatory context must stop locally: {completion:?}"
        );
        assert!(
            requests.requests().len() <= 1,
            "compaction cannot repeatedly resend irreducible required context"
        );
        assert!(
            completion.last_agent_message.is_none(),
            "no ordinary generation may run after overflow"
        );
        Ok(())
    })
}

#[tokio::test]
async fn failed_response_recording_is_reported_at_the_relay_boundary() {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    crate::session::tests::attach_thread_persistence(&mut session).await;
    session.live_thread().unwrap().shutdown().await.unwrap();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let recorder = OrderedResponseItemRecorder::default();
    let _barrier = recorder
        .enqueue(
            session,
            turn,
            ResponseItem::from(synthetic_tool_result("persist-fails")),
            Vec::new(),
            None,
        )
        .await;
    assert!(
        recorder.flush().await.is_err(),
        "failed response recording must be reported before publishing tool outputs"
    );
}

#[path = "completion_latency_tests.rs"]
mod completion_latency;

#[path = "request_latency_tests.rs"]
mod request_latency;
#[tokio::test]
async fn ctxaudit_pressure_retirement_counts_its_checkpoint() -> Result<()> {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.model_info.auto_compact_token_limit = Some(100_000);
    Arc::make_mut(&mut turn.config).model_auto_compact_token_limit_scope =
        codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
    seed_retirable_consumed_outputs(&session, 8).await;
    let (_, saved) = session
        .clone_history()
        .await
        .pressure_retirement_selection(CONTEXT_PRESSURE_KEPT_GENERATIONS, u64::MAX);
    assert!(saved > 0);
    let pressure = 100_000 + i64::try_from(saved)? - 1;
    assert!(
        !retire_consumed_tool_outputs_for_context_pressure(
            &session,
            &turn,
            CONTEXT_PRESSURE_KEPT_GENERATIONS,
            Some(ProjectedPromptPressure {
                total_tokens: pressure,
                auto_compact_scope_tokens: pressure
            })
        )
        .await
    );
    assert!(
        !crate::tool_history::ToolHistoryState::has_phase_checkpoint(
            session.clone_history().await.raw_items()
        )
    );
    Ok(())
}
#[tokio::test]
async fn audit_instruction_authority_unchanged_context_does_not_inflate_prompt_pressure() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let values = indexmap::IndexMap::from([(
        "policy".into(),
        codex_protocol::protocol::AdditionalContextEntry {
            kind: codex_protocol::protocol::AdditionalContextKind::Application,
            value: "Never edit protected.txt.".repeat(200),
        },
    )]);
    session
        .record_additional_context_snapshot(&turn, values.clone())
        .await
        .unwrap();
    let before = session.state.lock().await.additional_context.clone();
    assert_eq!(
        estimate_pending_additional_context_tokens(
            &session,
            &[TurnInput::AdditionalContext(values)]
        )
        .await,
        0
    );
    assert!(
        estimate_pending_additional_context_tokens(
            &session,
            &[TurnInput::AdditionalContext(indexmap::IndexMap::new())]
        )
        .await
            > 0
    );
    assert_eq!(session.state.lock().await.additional_context, before);
}
#[tokio::test]
async fn kd4_ip_009_scaffold_accounts_for_projected_desktop_payload() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.session_source = SessionSource::VSCode;
    let original = "<app-context>\n# Codex desktop context\n### Images/Visuals/Files\nKeep this guidance.\n### Automations\nDEFERRED_BODY_SHOULD_NOT_BE_SENT\n</app-context>";
    turn.developer_instructions = Some(original.to_string());
    let turn = Arc::new(turn);
    let step = StepContext::for_test(Arc::clone(&turn));
    let items = session
        .build_initial_context_with_world_state_and_mcp(
            &turn,
            &crate::context::world_state::WorldState::default(),
            false,
        )
        .await
        .items;
    let mut history = ContextManager::new();
    history.record_items(items.iter(), turn.model_info.truncation_policy.into());
    let prepared = history.prepare_for_sampling_prompt(
        &turn.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    let router = ToolRouter::from_parts(
        ToolRegistry::from_tools(std::iter::empty::<
            Arc<dyn crate::tools::registry::CoreToolRuntime>,
        >()),
        Vec::new(),
    );
    let prompt = build_projected_prompt(
        &session,
        &prepared,
        &router,
        &step,
        session.get_base_instructions().await,
    );
    let encoded = serde_json::to_string(&prompt.input).unwrap();
    assert!(!encoded.contains("DEFERRED_BODY_SHOULD_NOT_BE_SENT"));
    assert!(encoded.contains("context:desktop"));
    let measured = crate::context::PromptContextBreakdown::from_response_items(
        &prompt.input,
        &prompt.prompt_provenance,
    )
    .unwrap();
    assert!(
        measured.bytes(PromptContextCategory::AppDesktop) > 0,
        "the emitted payload, not the deferred source, belongs to Desktop"
    );
    assert_eq!(serde_json::to_string(&prompt.input).unwrap(), encoded);
}
#[tokio::test]
async fn router_reuse_tracks_same_count_native_environment_contracts() {
    use crate::session::turn_context::TurnEnvironment;
    use crate::tools::router::ToolRouterParams;
    use codex_utils_path_uri::PathUri;
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::SpawnCsv).unwrap();
    turn.config = Arc::new(config);
    let turn = Arc::new(turn);
    let base = StepContext::for_test(turn.clone());
    let local = base.environments.primary().unwrap().clone();
    let remote_environment = Arc::new(
        codex_exec_server::Environment::create_for_tests(Some("ws://127.0.0.1:1/catalog".into()))
            .unwrap(),
    );
    let remote = TurnEnvironment::new(
        "remote".into(),
        remote_environment.clone(),
        local.cwd().clone(),
        local.shell.clone(),
    );
    let make_router = |step: &StepContext, identity: ToolExposureIdentity| {
        ToolRouter::from_context(
            step,
            ToolRouterParams {
                mcp_tools: None,
                deferred_mcp_tools: None,
                tool_suggest_candidates: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &[],
                exposure_identity: identity,
            },
            &Default::default(),
        )
    };
    let identity = |current: DynamicToolExposureIdentity| ToolExposureIdentity {
        agent_surface_stage: current.agent_surface_stage,
        extension_tool_surface_revision: current.extension_tool_surface_revision,
        mcp_tool_catalog_revision: current.mcp_tool_catalog_revision,
        mcp_resources_available: current.mcp_resources_available,
        request_user_input_eligible: current.request_user_input_eligible,
        collaboration_mode: current.collaboration_mode,
        environment_mode: current.environment_mode,
        environment_starting: current.environment_starting,
        windows_shell_guidance: current.windows_shell_guidance,
        native_environment_contract: current.native_environment_contract,
        ..Default::default()
    };
    let local_router = make_router(
        &base,
        identity(current_dynamic_tool_exposure_identity(&session, &base).await),
    );
    assert!(local_router.has_registered_tool(&codex_tools::ToolName::plain("spawn_agents_on_csv")));
    assert!(finalized_router_matches_current_exposure(&session, &base, &local_router).await);
    let mut changed = (*base).clone();
    changed.environments.turn_environments = vec![remote.clone()];
    assert!(!finalized_router_matches_current_exposure(&session, &changed, &local_router).await);
    let remote_router = make_router(
        &changed,
        identity(current_dynamic_tool_exposure_identity(&session, &changed).await),
    );
    assert!(
        !remote_router.has_registered_tool(&codex_tools::ToolName::plain("spawn_agents_on_csv"))
    );
    assert!(!finalized_router_matches_current_exposure(&session, &base, &remote_router).await);

    let foreign_cwd = if cfg!(windows) {
        "file:///workspace"
    } else {
        "file:///C:/workspace"
    };
    let foreign = TurnEnvironment::new(
        "foreign".into(),
        remote_environment,
        PathUri::parse(foreign_cwd).unwrap(),
        local.shell.clone(),
    );
    changed.environments.turn_environments = vec![local, foreign];
    let ordered = make_router(
        &changed,
        identity(current_dynamic_tool_exposure_identity(&session, &changed).await),
    );
    changed.environments.turn_environments.reverse();
    assert!(!finalized_router_matches_current_exposure(&session, &changed, &ordered).await);
}
#[test]
fn accepted_final_identity_is_scoped_to_the_accepted_completion_attempt() -> Result<()> {
    run_turn_multi_thread_test_with_stack("accepted_final_identity", || async {
        crate::test_harness::require_network!();
        for (phases, expected) in [
            (
                vec![
                    (Some("final_answer"), "answer"),
                    (Some("commentary"), "progress"),
                ],
                Some("answer"),
            ),
            (
                vec![
                    (Some("commentary"), "progress"),
                    (Some("final_answer"), "answer"),
                ],
                Some("answer"),
            ),
            (
                vec![(None, "{\"answer\":\"α\"}\n")],
                Some("{\"answer\":\"α\"}\n"),
            ),
        ] {
            let server = responses::start_mock_server().await;
            let mut events = phases
                .into_iter()
                .enumerate()
                .map(|(index, (phase, text))| {
                    let mut event =
                        responses::ev_assistant_message(&format!("message-{index}"), text);
                    if let Some(phase) = phase {
                        event["item"]["phase"] = serde_json::json!(phase);
                    }
                    event
                })
                .collect::<Vec<_>>();
            events.push(responses::ev_completed("accepted-response"));
            let requests = responses::mount_sse_once(&server, responses::sse(events)).await;
            let test = test_codex().build(&server).await?;
            let completion = test
                .submit_turn_and_capture_completion("Finish the answer")
                .await?;
            let answer = completion
                .timing
                .as_ref()
                .unwrap()
                .final_answer
                .as_ref()
                .unwrap();
            assert_eq!(
                answer.message.as_ref().map(|message| message.text.as_str()),
                expected
            );
            if let Some(message) = &answer.message {
                assert!(
                    message
                        .completion_attempt_id
                        .starts_with(&format!("{}:", completion.turn_id))
                );
                assert!(message.item_id.starts_with("message-"));
            }
            assert_eq!(requests.requests().len(), 1);
        }
        let server = responses::start_mock_server().await;
        responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message("rejected", "rejected final"),
                    responses::ev_completed("rejected-response"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("accepted", "accepted correction"),
                    responses::ev_completed("accepted-response"),
                ]),
            ],
        )
        .await;
        let test = test_codex()
            .with_pre_build_hook(|home| {
                write_one_shot_stop_hook(home).unwrap();
            })
            .with_config(trust_discovered_hooks)
            .build(&server)
            .await?;
        let completion = test
            .submit_turn_and_capture_completion("Finish the answer")
            .await?;
        let accepted = completion
            .timing
            .as_ref()
            .unwrap()
            .final_answer
            .as_ref()
            .unwrap()
            .message
            .as_ref()
            .unwrap();
        assert_eq!(accepted.text, "accepted correction");

        // Exercise the real configured hook owner after its one-shot repair.
        // Its receipt must never cover an independently attributed mutation,
        // nor remove known validation failures or unknown-effect warnings.
        let session = &test.codex.as_ref().codex.session;
        let turn = session.new_default_turn().await;
        for changed_before_hook in [false, true] {
            let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
            let changed_path = test.workspace_path("independent-change.txt");
            if changed_before_hook {
                fs::write(&changed_path, "independent writer before hook")?;
                tracker
                    .lock()
                    .await
                    .record_unknown_mutation();
            }
            let report = run_completion_stop_hook(
                session,
                &turn,
                true,
                Some(accepted.text.clone()),
                || {
                    std::future::ready(codex_protocol::protocol::TurnCompletionAssessment {
                        failed_checks: vec!["required check still failed".into()],
                        ..Default::default()
                    })
                },
                &tracker,
                &CancellationToken::new(),
            )
            .await;
            let receipt = report
                .mutation_receipt
                .expect("approved command owns a mutation receipt");
            assert_eq!(
                receipt
                    .covers_only_hook_effect(0, tracker.lock().await.current_mutation_revision()),
                !changed_before_hook,
                "an earlier independent mutation is not the hook's own effect",
            );
            assert_eq!(
                report.assessment.failed_checks,
                vec!["required check still failed"]
            );
            assert!(!report.assessment.verification_gaps.is_empty());
            fs::write(&changed_path, "independent writer after hook")?;
            tracker
                .lock()
                .await
                .record_unknown_mutation();
            assert!(
                !receipt
                    .covers_only_hook_effect(0, tracker.lock().await.current_mutation_revision(),),
                "a later independent mutation must invalidate the receipt"
            );
        }
        let mut execution = TurnExecutionControl::new();
        let mut output = TurnOutputState::default();
        output.observe(
            None,
            Some(accepted.clone()),
            execution.baselines(0).relevant_state_fingerprint(),
            true,
            true,
        );
        execution.accepted_context_input(true);
        output.reconcile_revision(&execution.baselines(0).relevant_state_fingerprint());
        assert!(
            !output.has_answer(),
            "a hook receipt cannot restore a pre-correction answer"
        );
        Ok(())
    })
}

#[tokio::test]
async fn audit143_slow_history_commit_keeps_later_admitted_tools_polled() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let commit = session.durable_history_commit_gate.acquire().await.unwrap();
    let (later_tx, mut later_rx) = tokio::sync::oneshot::channel();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> = FuturesOrdered::new();
    in_flight.push_back(Box::pin(InFlightToolCall::from_test_future("first", Box::pin(async {
        Err(CodexErr::Fatal("first".into()))
    })).into_future()));
    in_flight.push_back(Box::pin(InFlightToolCall::from_test_future("second", Box::pin(async move {
        // A real asynchronous boundary makes this future pending on the first
        // collection poll; it must be polled again while the commit is blocked.
        tokio::task::yield_now().await;
        let _ = later_tx.send(());
        Err(CodexErr::Fatal("second".into()))
    })).into_future()));
    let drain = drain_in_flight(&mut in_flight, Arc::clone(&session), Arc::new(turn));
    tokio::pin!(drain);
    tokio::select! {
        result = &mut drain => panic!("commit must still be blocked: {result:?}"),
        _ = &mut later_rx => {}
        _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("later tool was starved by commit"),
    }
    drop(commit);
    assert!(matches!(drain.await, Err(CodexErr::Fatal(message)) if message == "first"));
    let history = session.clone_history().await;
    let calls = history.raw_items().iter().filter_map(|item| match item {
        ResponseItem::FunctionCallOutput { call_id, .. } if call_id == "first" || call_id == "second" => Some(call_id.as_str()),
        _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(calls, ["first", "second"]);
}
#[test]
fn audit150_skill_revisions_are_owner_bound_and_append_only_until_compaction() {
    let skill = |owner: char, body: &str| codex_core_skills::injection::SkillInjection {
        source_id:Some(owner.to_string().repeat(24)), name:"selected".into(),
        path:"/skills/shared/SKILL.md".into(),contents:body.into(),scope:codex_protocol::protocol::SkillScope::User,
    };
    let items=build_bounded_skill_context_items([
        &skill('a',"v1"),&skill('a',"v2"),&skill('b',"independent-provider"),&skill('a',"v1"),
    ]);
    assert!(items.iter().all(crate::stable_context::is_trusted_selected_skill_item));
    let sampling=crate::stable_context::project_sampling_history_context(items.clone().into());
    assert_eq!(sampling,items);
    let compacted=crate::stable_context::project_compaction_context(items.into());
    assert_eq!(compacted.len(),2);
    let text=response_input_texts(&compacted);
    assert!(text.iter().any(|body|body.contains("independent-provider")));
    assert!(text.iter().any(|body|body.contains("\nv1\n")));
    assert!(!text.iter().any(|body|body.contains("\nv2\n")));
}
