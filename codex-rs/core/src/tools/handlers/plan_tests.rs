use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tools::context::ToolCallSource;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use crate::tools::router::ToolRouterParams;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

fn plan_update_args(step: &str, status: StepStatus) -> UpdatePlanArgs {
    UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            step: step.to_string(),
            status,
        }],
    }
}

fn plan_arguments(step: &str, status: StepStatus) -> String {
    serde_json::to_string(&plan_update_args(step, status)).expect("serialize plan arguments")
}

fn assert_plan_output_schema(response: &serde_json::Value) {
    let definition =
        codex_tools::tool_spec_to_code_mode_tool_definition(&create_update_plan_tool())
            .expect("plan tool definition");
    assert!(definition.description.contains("no_progress: boolean"));
    let ToolSpec::Function(spec) = create_update_plan_tool() else {
        panic!("plan uses a function spec");
    };
    let validator = jsonschema::validator_for(spec.output_schema.as_ref().expect("output schema"))
        .expect("valid plan output schema");
    assert!(
        validator.is_valid(response),
        "invalid plan output: {response}"
    );
    for (pointer, invalid) in [
        ("/effect", serde_json::json!("unknown")),
        ("/no_progress", serde_json::json!("false")),
        ("/current_plan/plan/0/status", serde_json::json!("done")),
    ] {
        let mut malformed = response.clone();
        *malformed.pointer_mut(pointer).expect("response field") = invalid;
        assert!(!validator.is_valid(&malformed), "accepted {malformed}");
    }
}

#[test]
fn plan_output_signals_governor_state() {
    let output = PlanToolOutput {
        current_plan: UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        },
        effect: PlanUpdateEffect::NoOp,
        governor_plan: None,
    };

    let signal = output
        .sampling_request_signal()
        .expect("plan updates should signal governor state");
    assert_eq!(signal["kind"], "plan_update");
    assert_eq!(signal["effect"], "no_op");
    assert_eq!(signal["no_progress"], true);
    assert!(signal["plan"].is_null());
    assert_eq!(signal["unfinished_mutation_obligation"], false);
}

#[test]
fn unchanged_plan_output_remains_compact() {
    let output = PlanToolOutput {
        current_plan: UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        },
        effect: PlanUpdateEffect::NoOp,
        governor_plan: None,
    };
    let payload = ToolPayload::Function {
        arguments: r#"{"plan":[]}"#.to_string(),
    };
    let ResponseInputItem::FunctionCallOutput {
        output: response, ..
    } = output.to_response_item("unchanged-plan-output", &payload)
    else {
        panic!("plan update should return function output");
    };
    let FunctionCallOutputBody::Text(text) = response.body else {
        panic!("plan update should return text output");
    };
    let response = serde_json::from_str::<serde_json::Value>(&text).expect("plan output JSON");

    assert_eq!(response["message"], PLAN_UNCHANGED_MESSAGE);
    assert_eq!(response["effect"], "no_op");
    assert_eq!(response["no_progress"], true);
    assert_eq!(response["current_plan"]["plan"], serde_json::json!([]));
    assert_eq!(response.as_object().expect("object response").len(), 4);
    assert_eq!(output.code_mode_result(&payload), response);
}

#[test]
fn update_plan_schema_is_the_simple_checklist_contract() {
    let tool = serde_json::to_value(create_update_plan_tool()).expect("serialize update_plan");
    assert!(
        tool["parameters"]["properties"]["plan"]["description"]
            .as_str()
            .unwrap()
            .contains("replacing the previous plan. Omitted steps are removed")
    );
    assert!(
        tool["parameters"]["properties"]["explanation"]["description"]
            .as_str()
            .unwrap()
            .contains("Do not mark unfinished work completed")
    );
    let properties = tool["parameters"]["properties"]
        .as_object()
        .expect("top-level checklist properties");
    let item_properties = tool["parameters"]["properties"]["plan"]["items"]["properties"]
        .as_object()
        .expect("checklist item properties");
    let statuses =
        tool["parameters"]["properties"]["plan"]["items"]["properties"]["status"]["enum"]
            .as_array()
            .expect("status enum");

    assert_eq!(
        properties.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["explanation", "plan", "set"]
    );
    assert_eq!(
        item_properties
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["status", "step"]
    );
    assert_eq!(
        statuses,
        &vec![
            serde_json::json!("pending"),
            serde_json::json!("in_progress"),
            serde_json::json!("completed"),
        ]
    );
    let validator = jsonschema::validator_for(&tool["parameters"]).unwrap();
    assert!(validator.is_valid(&serde_json::json!({"plan": []})));
    assert!(validator.is_valid(&serde_json::json!({"set": [{"index": 0, "status": "completed"}]})));
    for invalid in [
        serde_json::json!({}),
        serde_json::json!({"plan": [], "set": [{"index": 0, "status": "completed"}]}),
        serde_json::json!({"set": []}),
        serde_json::json!({"set": [{"index": -1, "status": "completed"}]}),
        serde_json::json!({"set": [{"index": 0, "status": "done"}]}),
    ] {
        assert!(!validator.is_valid(&invalid), "accepted {invalid}");
    }
    assert_eq!(
        tool.pointer("/parameters/properties/plan/items/required"),
        Some(&serde_json::json!(["step", "status"]))
    );
    assert_eq!(
        tool.pointer("/parameters/additionalProperties"),
        Some(&serde_json::json!(false))
    );
    assert_eq!(
        tool.pointer("/parameters/properties/plan/items/additionalProperties"),
        Some(&serde_json::json!(false))
    );
}

#[tokio::test]
async fn plan_updates_use_session_checklist_store_and_preserve_governor_effects() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let handler = PlanHandler;

    let initial_payload = ToolPayload::Function {
        arguments: plan_arguments("Implement the change", StepStatus::InProgress),
    };
    let initial = handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "plan-initial".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: initial_payload.clone(),
        })
        .await
        .expect("initial checklist update");
    assert_eq!(
        initial.code_mode_result(&initial_payload)["effect"],
        "initial"
    );
    assert!(
        initial
            .sampling_request_signal()
            .expect("initial governor signal")["plan"]
            .is_object()
    );

    let completed_payload = ToolPayload::Function {
        arguments: plan_arguments("Implement the change", StepStatus::Completed),
    };
    let completed = handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "plan-completed".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: completed_payload.clone(),
        })
        .await
        .expect("completed checklist update");
    assert_eq!(
        completed.code_mode_result(&completed_payload)["effect"],
        "status_only"
    );
    assert!(
        completed
            .sampling_request_signal()
            .expect("status-only governor signal")["plan"]
            .is_null()
    );

    let repeated = handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(turn),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "plan-no-op".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: completed_payload.clone(),
        })
        .await
        .expect("repeated checklist update");
    assert_eq!(
        repeated.code_mode_result(&completed_payload)["effect"],
        "no_op"
    );

    let current = session
        .services
        .plan_store
        .current_for_test()
        .await
        .expect("stored checklist");
    assert_eq!(current.plan[0].status, StepStatus::Completed);
    for (output, payload) in [
        (&initial, &initial_payload),
        (&completed, &completed_payload),
        (&repeated, &completed_payload),
    ] {
        assert_plan_output_schema(&output.code_mode_result(payload));
    }
}

#[tokio::test]
async fn update_plan_rejects_unknown_arguments_at_runtime() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    for (call_id, arguments) in [
        (
            "removed-root-field",
            serde_json::json!({"unexpected": true, "plan": []}).to_string(),
        ),
        (
            "removed-item-field",
            serde_json::json!({
                "plan": [{"unexpected": true, "step": "work", "status": "pending"}]
            })
            .to_string(),
        ),
    ] {
        let result = PlanHandler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: call_id.to_string(),
                tool_name: ToolName::plain("update_plan"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function { arguments },
            })
            .await;

        assert!(matches!(
            result,
            Err(FunctionCallError::RespondToModel(message)) if message.contains("unknown field")
        ));
    }
    assert!(
        session
            .services
            .plan_store
            .current_for_test()
            .await
            .is_none()
    );
}

#[tokio::test]
async fn status_deltas_preserve_steps_and_reject_invalid_updates_atomically() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let initial = UpdatePlanArgs {
        explanation: Some("retained explanation".to_string()),
        plan: vec![
            PlanItemArg {
                step: "Implement".to_string(),
                status: StepStatus::InProgress,
            },
            PlanItemArg {
                step: "Verify".to_string(),
                status: StepStatus::Pending,
            },
        ],
    };
    for (arguments, has_plan, cancelled, error) in [
        (
            serde_json::json!({"set": [{"index": 0, "status": "completed"}]}),
            false,
            false,
            "create a plan",
        ),
        (serde_json::json!({"set": []}), true, false, "at least one"),
        (
            serde_json::json!({"set": [{"index": 0, "status": "completed"}, {"index": 2, "status": "pending"}]}),
            true,
            false,
            "out of range",
        ),
        (
            serde_json::json!({"set": [{"index": 0, "status": "completed"}, {"index": 0, "status": "pending"}]}),
            true,
            false,
            "duplicate",
        ),
        (
            serde_json::json!({"set": [{"index": 1, "status": "in_progress"}]}),
            true,
            false,
            "at most one",
        ),
        (
            serde_json::json!({"plan": [], "set": []}),
            true,
            false,
            "exactly one",
        ),
        (serde_json::json!({}), true, false, "exactly one"),
        (
            serde_json::json!({"set": [{"index": 0, "status": "completed", "unexpected": true}]}),
            true,
            false,
            "unknown field",
        ),
        (
            serde_json::json!({"set": [{"index": 0, "status": "completed"}]}),
            true,
            true,
            "cancelled",
        ),
    ] {
        let expected = has_plan.then(|| initial.clone());
        session.services.plan_store.restore(expected.clone()).await;
        let token = CancellationToken::new();
        if cancelled {
            token.cancel();
        }
        let result = PlanHandler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                cancellation_token: token,
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "invalid-delta".to_string(),
                tool_name: ToolName::plain("update_plan"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
            })
            .await;
        assert!(
            matches!(result, Err(FunctionCallError::RespondToModel(ref message)) if message.contains(error)),
            "expected {error}"
        );
        assert_eq!(
            session.services.plan_store.current_for_test().await,
            expected
        );
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event.msg, EventMsg::PlanUpdate(_)));
        }
    }
    session
        .services
        .plan_store
        .restore(Some(initial.clone()))
        .await;
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({"set": [{"index": 0, "status": "completed"}, {"index": 1, "status": "in_progress"}]}).to_string(),
    };
    let mut expected = initial;
    expected.plan[0].status = StepStatus::Completed;
    expected.plan[1].status = StepStatus::InProgress;
    for effect in ["status_only", "no_op"] {
        let result = PlanHandler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                step_context: StepContext::for_test(Arc::clone(&turn)),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: format!("delta-{effect}"),
                tool_name: ToolName::plain("update_plan"),
                source: ToolCallSource::Direct,
                payload: payload.clone(),
            })
            .await
            .unwrap();
        let output = result.code_mode_result(&payload);
        assert_plan_output_schema(&output);
        assert_eq!(output["effect"], effect);
        assert_eq!(output["current_plan"], serde_json::json!(expected));
        assert!(result.sampling_request_signal().unwrap()["plan"].is_null());
        assert_eq!(
            session.services.plan_store.current_for_test().await,
            Some(expected.clone())
        );
        assert!(
            matches!(events.recv().await.unwrap().msg, EventMsg::PlanUpdate(ref plan) if plan == &expected)
        );
    }
}

#[tokio::test]
async fn cancellation_before_plan_commit_does_not_emit_plan_update() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();
    let invocation = ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(turn),
        cancellation_token,
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "cancelled-plan".to_string(),
        tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: plan_arguments("Do not commit this plan", StepStatus::Pending),
        },
    };

    let result = PlanHandler.handle(invocation).await;

    assert!(matches!(
        result,
        Err(FunctionCallError::RespondToModel(message))
            if message == "update_plan was cancelled before the plan update began"
    ));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.msg, EventMsg::PlanUpdate(_)),
            "a pre-commit cancellation must not emit a plan update"
        );
    }
    assert!(
        session
            .services
            .plan_store
            .current_for_test()
            .await
            .is_none()
    );
}

#[tokio::test]
async fn registered_plan_output_restores_after_history_serialization() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = Arc::new(ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    assert!(step_context.set_tool_router(router).is_ok());
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step_context,
        Arc::new(Mutex::new(TurnDiffTracker::new())),
    );
    runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: ToolName::plain("update_plan"),
                call_id: "plan-before-delta".to_string(),
                payload: ToolPayload::Function {
                    arguments: plan_arguments(
                        "Restore the accepted checklist",
                        StepStatus::InProgress,
                    ),
                },
            },
            CancellationToken::new(),
        )
        .await
        .expect("initial plan");
    let arguments = serde_json::json!({"set": [{"index": 0, "status": "completed"}]}).to_string();
    let response = runtime
        .handle_tool_call(
            ToolCall {
                tool_name: ToolName::plain("update_plan"),
                call_id: "plan-replay".to_string(),
                payload: ToolPayload::Function {
                    arguments: arguments.clone(),
                },
            },
            CancellationToken::new(),
        )
        .await
        .expect("registered plan update");
    let ResponseInputItem::FunctionCallOutput { call_id, output } = response else {
        panic!("plan response must be a function output");
    };
    let value = serde_json::from_str(&output.body.to_text().expect("plan output text"))
        .expect("plan output JSON");
    assert_plan_output_schema(&value);
    assert_eq!(value["effect"], "status_only");
    let history = vec![
        codex_protocol::models::ResponseItem::FunctionCall {
            id: None,
            name: "update_plan".to_string(),
            namespace: None,
            arguments,
            call_id: call_id.clone(),
            internal_chat_message_metadata_passthrough: None,
        },
        codex_protocol::models::ResponseItem::FunctionCallOutput {
            id: None,
            call_id,
            output,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let serialized = serde_json::to_string(&history).expect("serialize persisted history");
    let history = serde_json::from_str::<Vec<codex_protocol::models::ResponseItem>>(&serialized)
        .expect("deserialize persisted history");
    let restored = crate::plan_store::PlanStore::default();
    assert!(restored.restore_from_history(&history).await);
    let expected = plan_update_args("Restore the accepted checklist", StepStatus::Completed);
    assert_eq!(restored.current_for_test().await, Some(expected.clone()));
    assert_eq!(
        restored.update(expected).await.effect,
        PlanUpdateEffect::NoOp
    );
}

#[tokio::test]
async fn registered_plan_rejects_multiple_active_steps_without_mutation_or_event() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let step_context = StepContext::for_test(turn);
    let router = Arc::new(ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    assert!(step_context.set_tool_router(router).is_ok());
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step_context,
        Arc::new(Mutex::new(TurnDiffTracker::new())),
    );
    let accepted = plan_update_args("Implement the fix", StepStatus::InProgress);
    let mut rejected = accepted.clone();
    rejected.plan.push(PlanItemArg {
        step: "Verify the fix".to_string(),
        status: StepStatus::InProgress,
    });
    for (call_id, args, expected_success) in [
        ("accepted-plan", accepted.clone(), true),
        ("rejected-plan", rejected, false),
    ] {
        let response = runtime
            .clone()
            .handle_tool_call(
                ToolCall {
                    tool_name: ToolName::plain("update_plan"),
                    call_id: call_id.to_string(),
                    payload: ToolPayload::Function {
                        arguments: serde_json::to_string(&args).expect("serialize plan"),
                    },
                },
                CancellationToken::new(),
            )
            .await
            .expect("tool response");
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            panic!("expected function output");
        };
        assert_eq!(output.success, Some(expected_success));
        if !expected_success {
            assert!(
                output
                    .body
                    .to_text()
                    .expect("error text")
                    .contains("at most one in_progress")
            );
        }
        assert_eq!(
            session.services.plan_store.current_for_test().await,
            Some(accepted.clone())
        );
        let plan_events = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| matches!(event.msg, EventMsg::PlanUpdate(_)))
            .count();
        assert_eq!(plan_events, usize::from(expected_success));
    }
}

#[tokio::test]
async fn cancellation_after_plan_commit_boundary_waits_for_session_update() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let call_id = "cancelled-after-plan-commit-boundary";
    let hook = PlanCommitBoundaryHook::install(call_id);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = Arc::new(ToolRouter::from_context(
        step_context.as_ref(),
        ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    assert!(step_context.set_tool_router(router).is_ok());
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step_context,
        Arc::new(Mutex::new(TurnDiffTracker::new())),
    );
    let cancellation_token = CancellationToken::new();
    let call = ToolCall {
        tool_name: ToolName::plain("update_plan"),
        call_id: call_id.to_string(),
        payload: ToolPayload::Function {
            arguments: plan_arguments(
                "Commit this plan before returning cancellation",
                StepStatus::Pending,
            ),
        },
    };
    let response_task = runtime.handle_tool_call(call, cancellation_token.clone());
    tokio::pin!(response_task);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    tokio::time::timeout_at(deadline, async {
        tokio::select! {
            _ = hook.wait_until_reached() => {},
            result = &mut response_task => panic!("plan returned before commit boundary: {result:?}"),
        }
    })
        .await
        .expect("plan handler should reach its commit boundary");

    cancellation_token.cancel();
    tokio::time::timeout_at(deadline, async {
        tokio::select! {
            _ = hook.cancellation_observed.notified() => {},
            result = &mut response_task => panic!("plan returned before cancellation cleanup: {result:?}"),
        }
    })
    .await
    .expect("real runtime cancellation should reach the admitted handler");
    assert!(
        futures::poll!(&mut response_task).is_pending(),
        "runtime cancellation must wait for commit cleanup"
    );
    hook.release();

    let response = tokio::time::timeout_at(deadline, &mut response_task)
        .await
        .expect("cancelled plan call should finish after commit")
        .expect("plan runtime should return a response");
    let ResponseInputItem::FunctionCallOutput {
        call_id: response_id,
        output,
    } = response
    else {
        panic!("cancelled plan tool should return function output");
    };
    assert_eq!(response_id, call_id);
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("cancelled plan tool output should be text");
    };
    assert!(text.contains("aborted by user"));

    let mut updates = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::PlanUpdate(update) = event.msg {
            assert_eq!(event.id, turn.sub_id);
            updates.push(update);
        }
    }
    assert_eq!(
        updates.len(),
        1,
        "exactly one update must already exist at response completion"
    );
    let plan_update = &updates[0];
    assert_eq!(
        plan_update.plan,
        vec![PlanItemArg {
            step: "Commit this plan before returning cancellation".to_string(),
            status: StepStatus::Pending,
        }]
    );
    assert_eq!(plan_update.explanation, None);
    let stored = session
        .services
        .plan_store
        .current_for_test()
        .await
        .expect("plan should commit before cancellation response");
    assert_eq!(stored.plan, plan_update.plan);
}
