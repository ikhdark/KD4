use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tools::context::ToolCallSource;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::timeout;
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
        vec!["explanation", "plan"]
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
    assert_eq!(
        tool.pointer("/parameters/required"),
        Some(&serde_json::json!(["plan"]))
    );
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

#[test]
fn update_plan_waits_for_runtime_cancellation_commit_cleanup() {
    assert!(PlanHandler.waits_for_runtime_cancellation());
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
async fn cancellation_after_plan_commit_boundary_waits_for_session_update() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let call_id = "cancelled-after-plan-commit-boundary";
    let hook = PlanCommitBoundaryHook::install(call_id);
    let handler = Arc::new(PlanHandler) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
    ));
    let step_context = StepContext::for_test(Arc::clone(&turn)).with_tool_router_for_test(router);
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
    let mut response_task =
        tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
    timeout(Duration::from_secs(2), hook.wait_until_reached())
        .await
        .expect("plan handler should reach its commit boundary");

    cancellation_token.cancel();
    assert!(
        timeout(Duration::from_millis(50), &mut response_task)
            .await
            .is_err(),
        "runtime cancellation must wait for commit cleanup"
    );
    hook.release();

    let response = timeout(Duration::from_secs(2), &mut response_task)
        .await
        .expect("cancelled plan call should finish after commit")
        .expect("plan response task should join")
        .expect("plan runtime should return a response");
    let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
        panic!("cancelled plan tool should return function output");
    };
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("cancelled plan tool output should be text");
    };
    assert!(text.contains("aborted by user"));

    let plan_update = timeout(Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("event channel should remain open");
            if let EventMsg::PlanUpdate(update) = event.msg {
                break update;
            }
        }
    })
    .await
    .expect("plan update event should be emitted before cancellation completes");
    assert_eq!(
        plan_update.plan[0].step,
        "Commit this plan before returning cancellation"
    );
    let stored = session
        .services
        .plan_store
        .current_for_test()
        .await
        .expect("plan should commit before cancellation response");
    assert_eq!(stored.plan, plan_update.plan);
}
