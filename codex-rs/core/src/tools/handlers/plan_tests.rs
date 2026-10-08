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

fn status_arguments(step: &str, status: StepStatus) -> String {
    serde_json::json!({"set": [{
        "step_id": crate::plan_store::plan_step_id(step), "status": status,
    }]}).to_string()
}

#[tokio::test]
async fn plan_replacement_uses_the_captured_sampling_fence() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    session.services.plan_store.update(plan_update_args("work", StepStatus::Pending)).await;
    let step = session.capture_step_context(Arc::clone(&turn)).await.unwrap();
    let invoke = |step_context, arguments| ToolInvocation {
        session: Arc::clone(&session), step_context,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "sampling-fence".into(), tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct, payload: ToolPayload::Function { arguments },
    };
    // No-op publication does not invalidate the sampled state.
    PlanHandler.handle(invoke(Arc::clone(&step), plan_arguments("work", StepStatus::Pending))).await.unwrap();
    PlanHandler.handle(invoke(Arc::clone(&step), plan_arguments("work", StepStatus::InProgress))).await.unwrap();
    // A second direct/nested call from the same sample must reconcile first.
    assert!(PlanHandler.handle(invoke(Arc::clone(&step), plan_arguments("work", StepStatus::Completed))).await.is_err());
    let revision = session.services.plan_store.execution_snapshot().await.unwrap().revision;
    let mut args = serde_json::to_value(plan_update_args("work", StepStatus::Completed)).unwrap();
    args["expected_revision"] = revision.into();
    PlanHandler.handle(invoke(step, args.to_string())).await.unwrap();
    let next = session.capture_step_context(turn).await.unwrap();
    assert!(PlanHandler.handle(invoke(Arc::clone(&next), args.to_string())).await.is_err(), "explicit stale CAS is never bypassed");
    assert!(PlanHandler.handle(invoke(Arc::clone(&next), serde_json::json!({"set":[{"index":0,"status":"pending"}]}).to_string())).await.is_err(), "index updates still require a revision");
    PlanHandler.handle(invoke(next, plan_arguments("work", StepStatus::Completed))).await.unwrap();
}

#[tokio::test]
async fn plan_publication_is_ordered_and_cancelled_waiter_cannot_publish() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let invoke = |call_id: &str, arguments: String, cancellation_token| ToolInvocation {
        session: Arc::clone(&session), step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token, tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.into(), tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct, payload: ToolPayload::Function { arguments },
    };
    let hook = PlanCommitBoundaryHook::install("ordered-plan-initial");
    let first = PlanHandler.handle(invoke("ordered-plan-initial", plan_arguments("ordered work", StepStatus::Pending), CancellationToken::new()));
    tokio::pin!(first);
    tokio::select! {
        _ = hook.wait_until_reached() => {},
        result = &mut first => panic!("early first publication: {}", result.is_ok()),
    }
    let cancelled = CancellationToken::new();
    let waiter = PlanHandler.handle(invoke("ordered-plan-cancelled", status_arguments("ordered work", StepStatus::Completed), cancelled.clone()));
    tokio::pin!(waiter);
    assert!(futures::poll!(&mut waiter).is_pending());
    cancelled.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(2), &mut waiter).await.unwrap().is_err());
    let second = PlanHandler.handle(invoke("ordered-plan-final", status_arguments("ordered work", StepStatus::Completed), CancellationToken::new()));
    tokio::pin!(second);
    assert!(futures::poll!(&mut second).is_pending());
    hook.release();
    first.await.unwrap();
    second.await.unwrap();
    let statuses = std::iter::from_fn(|| events.try_recv().ok()).filter_map(|event| match event.msg {
        EventMsg::PlanUpdate(plan) => Some(plan.plan[0].status), _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(statuses, vec![StepStatus::Pending, StepStatus::Completed]);
}

#[tokio::test]
async fn plan_scope_change_uses_accepted_user_input_not_context_text() {
    use crate::session::TurnInput;
    use codex_protocol::models::{ContentItem, ResponseItem};
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let original = plan_update_args("Deploy", StepStatus::Pending);
    session.services.plan_store.update(original.clone()).await;
    let id = crate::plan_store::plan_step_id("Deploy");
    let instruction = "Do not deploy; review only.";
    let input = TurnInput::ResponseItem(ResponseItem::Message {
        id: None, role: "user".into(), content: vec![ContentItem::InputText { text: instruction.into() }],
        phase: None, internal_chat_message_metadata_passthrough: None,
    });
    crate::hook_runtime::record_pending_input(&session, &turn, input, Vec::new()).await.unwrap();
    let args = serde_json::json!({"expected_revision":crate::plan_store::plan_revision(Some(&original)),
        "plan":[],"superseded":[{"step_id":id,"reason":"User cancelled deployment"}],
        "scope_change_instructions":{(id):instruction}});
    let invoke = || ToolInvocation {
        session: Arc::clone(&session), step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: CancellationToken::new(), tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "user-grounded-supersession".into(), tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct, payload: ToolPayload::Function { arguments: args.to_string() },
    };
    assert!(PlanHandler.handle(invoke()).await.is_err());
    crate::hook_runtime::record_pending_input(&session, &turn, TurnInput::UserInput {
        content: vec![codex_protocol::user_input::UserInput::Text { text: instruction.into(), text_elements: vec![] }], client_id: None,
    }, Vec::new()).await.unwrap();
    let result = PlanHandler.handle(invoke()).await.unwrap().code_mode_result(&ToolPayload::Function { arguments: args.to_string() });
    assert_plan_output_schema(&result);
    assert_eq!(result["obligations"]["superseded"], 1);
    assert_eq!(result["completion_authority"], "checklist_only");
}

#[tokio::test]
async fn plan_active_response_is_bounded_while_durable_history_is_complete() {
    let store = crate::plan_store::PlanStore::default();
    for index in 0..40 {
        let args = serde_json::json!({"expected_revision":store.execution_snapshot().await.map(|snapshot|snapshot.revision),
            "plan":[{"step":format!("finished {index}"),"status":"completed"}]});
        store.update_tool(serde_json::from_value(args).unwrap()).await.unwrap();
    }
    let (plan, lineage) = store.snapshot_with_lineage().await.unwrap();
    let output = PlanToolOutput { current_plan: plan, lineage, effect: PlanUpdateEffect::NoOp };
    let active = output.response_result();
    let durable = output.durable_response();
    assert_plan_output_schema(&active);
    assert_plan_output_schema(&durable);
    assert_eq!(active["lineage_complete"], false);
    assert_eq!(durable["lineage_complete"], true);
    assert!(active.to_string().len() * 3 < durable.to_string().len());
    assert_eq!(active["revision"], durable["revision"]);
    let restored: crate::plan_store::PlanToolResponse = serde_json::from_value(durable).unwrap();
    store.restore_with_lineage(Some(restored.current_plan), Some(restored.lineage)).await;
    assert_eq!(store.snapshot_with_lineage().await.unwrap().1.requirements.len(), 40);
}

#[tokio::test]
async fn restored_orphans_can_be_reattached_or_superseded_through_the_tool() {
    for supersede in [false, true] {
        let (session, turn, _events) = make_session_and_context_with_rx().await;
        let plan = plan_update_args("current", StepStatus::Completed);
        let mut lineage = crate::plan_store::PlanLineage::default();
        lineage.requirements.insert("orphan".into(), crate::plan_store::PlanRequirement {
            text: "original remaining work".into(), status: StepStatus::Pending, superseded_reason: None,
        });
        session.services.plan_store.restore_with_lineage(Some(plan), Some(lineage)).await;
        let revision = session.services.plan_store.execution_snapshot().await.unwrap().revision;
        let mut args = serde_json::json!({
            "expected_revision": revision,
            "plan": [{"step": "current", "status": "completed"}],
        });
        if supersede {
            args["superseded"] = serde_json::json!([{"step_id": "orphan", "reason": "User removed this scope"}]);
        } else {
            args["plan"][0]["step"] = serde_json::json!("original remaining work");
            args["plan"][0]["continues"] = serde_json::json!(["orphan"]);
        }
        let payload = ToolPayload::Function { arguments: args.to_string() };
        let result = PlanHandler.handle(ToolInvocation {
            session: Arc::clone(&session), step_context: StepContext::for_test(turn),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: format!("resolve-orphan-{supersede}"), tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct, payload: payload.clone(),
        }).await.unwrap();
        let output = result.code_mode_result(&payload);
        assert_plan_output_schema(&output);
        // A proposed supersession is not a verified user instruction.
        assert_eq!(output["obligations"]["unresolved"],
            if supersede { serde_json::json!(["orphan"]) } else { serde_json::json!([]) });
        assert_eq!(output["completion_authority"], "checklist_only");
        let (_, lineage) = session.services.plan_store.snapshot_with_lineage().await.unwrap();
        assert_eq!(lineage.requirements["orphan"].superseded_reason.is_some(), supersede);
        if !supersede {
            assert_eq!(lineage.requirements["orphan"].status, StepStatus::Completed);
        }
    }
}

#[tokio::test]
async fn published_noop_skips_durable_write_and_event_but_restore_requires_publication() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    crate::session::tests::attach_thread_persistence(Arc::get_mut(&mut session).unwrap()).await;
    let invoke = |arguments: String| ToolInvocation {
        session: Arc::clone(&session), step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: CancellationToken::new(), tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "publication-proof".into(), tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct, payload: ToolPayload::Function { arguments },
    };
    PlanHandler.handle(invoke(plan_arguments("work", StepStatus::Pending))).await.unwrap();
    assert_eq!(std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| matches!(event.msg, EventMsg::PlanUpdate(_))).count(), 1);
    let before = session.services.plan_store.snapshot_with_lineage().await.unwrap();
    session.live_thread().unwrap().shutdown().await.unwrap();
    // A durable write would now fail. This proves the ordinary no-op bypasses it.
    let output = PlanHandler.handle(invoke(status_arguments("work", StepStatus::Pending))).await.unwrap();
    assert_eq!(output.log_preview(), PLAN_UNCHANGED_MESSAGE);
    assert!(events.try_recv().is_err());
    assert_eq!(session.services.plan_store.snapshot_with_lineage().await.unwrap(), before);
    session.services.plan_store.restore_with_lineage(Some(before.0), Some(before.1)).await;
    let error = PlanHandler.handle(invoke(status_arguments("work", StepStatus::Pending))).await;
    assert!(matches!(error, Err(FunctionCallError::RespondToModel(message))
        if message.contains("durable plan publication failed")));
}

#[tokio::test]
async fn failed_plan_publication_preserves_plan_and_revision() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    crate::session::tests::attach_thread_persistence(
        Arc::get_mut(&mut session).expect("unique session"),
    ).await;
    session.services.plan_store.update(plan_update_args("retained", StepStatus::Pending)).await;
    let before = session.services.plan_store.snapshot_with_lineage().await;
    session.live_thread().unwrap().shutdown().await.unwrap();
    let result = PlanHandler.handle(ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(turn),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "failed-plan-publication".into(),
        tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: status_arguments("retained", StepStatus::Completed),
        },
    }).await;
    assert!(matches!(result, Err(FunctionCallError::RespondToModel(message))
        if message.contains("previous plan and revision are unchanged")));
    assert_eq!(session.services.plan_store.snapshot_with_lineage().await, before);
    assert!(!std::iter::from_fn(|| events.try_recv().ok())
        .any(|event| matches!(event.msg, EventMsg::PlanUpdate(_))));
}

#[tokio::test]
async fn cancellation_while_waiting_for_plan_lock_preserves_revision_and_retry_publishes_once() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    session.services.plan_store.update(plan_update_args("retained", StepStatus::Pending)).await;
    let before = session.services.plan_store.snapshot_with_lineage().await;
    let arguments = status_arguments("retained", StepStatus::Completed);
    let staged = session.services.plan_store.stage_tool(
        serde_json::from_str(&arguments).unwrap(),
    ).await.unwrap();
    let token = CancellationToken::new();
    let invocation = |token| ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: token,
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "cancelled-waiting-for-plan-lock".into(),
        tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function { arguments: arguments.clone() },
    };
    let mut request = PlanHandler.handle(invocation(token.clone()));
    assert!(futures::poll!(&mut request).is_pending());
    token.cancel();
    drop(staged);
    assert!(request.await.is_err());
    assert_eq!(session.services.plan_store.snapshot_with_lineage().await, before);
    assert!(events.try_recv().is_err());
    PlanHandler.handle(invocation(CancellationToken::new())).await.unwrap();
    assert_eq!(session.services.plan_store.snapshot().await,
        Some(plan_update_args("retained", StepStatus::Completed)));
    assert_eq!(std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| matches!(event.msg, EventMsg::PlanUpdate(_))).count(), 1);
}

#[tokio::test]
async fn persisted_plan_response_omits_redundant_lineage() {
    let store = crate::plan_store::PlanStore::default();
    let update = store.update(plan_update_args("inspect", StepStatus::Pending)).await;
    let output = PlanToolOutput { current_plan: update.current, effect: update.effect, lineage: update.lineage };
    let response = output.response_result();
    assert!(response.get("lineage").is_none());
    assert_eq!(response["step_ids"][0].as_str().unwrap().len(), 16);
    assert_plan_output_schema(&response);
    let snapshot = crate::plan_store::plan_snapshot_item(&response);
    let restored = crate::plan_store::plan_snapshot_from_item(&snapshot).unwrap();
    let resumed = crate::plan_store::PlanStore::default();
    resumed.restore_with_lineage(Some(restored.current_plan), Some(restored.lineage)).await;
    assert_eq!(resumed.snapshot_with_lineage().await.unwrap().1.obligation_summary(&output.current_plan).unresolved,
        vec![response["step_ids"][0].as_str().unwrap().to_string()]);
}

fn assert_plan_output_schema(response: &serde_json::Value) {
    let definition =
        codex_tools::tool_spec_to_code_mode_tool_definition(&create_update_plan_tool())
            .expect("plan tool definition");
    assert!(definition.description.contains("no_progress: boolean"));
    let ToolSpec::Function(spec) = create_update_plan_tool() else {
        panic!("plan uses a function spec");
    };
    let validator = jsonschema::validator_for(&spec.output_schema.as_ref().expect("output schema").to_value())
        .expect("valid plan output schema");
    assert!(
        validator.is_valid(response),
        "invalid plan output: {response}"
    );
    for (pointer, invalid) in [
        ("/effect", serde_json::json!("unknown")),
        ("/no_progress", serde_json::json!("false")),
        ("/current_plan/plan", serde_json::json!([{"step":"invalid status", "status":"done"}])),
    ] {
        let mut malformed = response.clone();
        *malformed.pointer_mut(pointer).expect("response field") = invalid;
        assert!(!validator.is_valid(&malformed), "accepted {malformed}");
    }
}

#[test]
fn plan_output_signals_governor_state() {
    let output = PlanToolOutput {
        lineage: Default::default(),
        current_plan: UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        },
        effect: PlanUpdateEffect::NoOp,
    };

    let signal = output
        .sampling_request_signal()
        .expect("plan updates should signal governor state");
    assert_eq!(signal["kind"], "plan_update");
    assert_eq!(signal["effect"], "no_op");
    assert_eq!(signal["no_progress"], true);
    assert_eq!(signal["plan"], serde_json::json!(output.current_plan));
}

#[tokio::test]
async fn plan_mode_rejects_updates_without_side_effects() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    turn.collaboration_mode.mode = ModeKind::Plan;
    session.services.plan_store.update(plan_update_args("interrupted implementation", StepStatus::Pending)).await;
    let before = session.services.plan_store.current_for_test().await;
    let result = PlanHandler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::new(turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "plan-mode-update".into(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: plan_arguments("forbidden", StepStatus::InProgress),
            },
        })
        .await;
    assert!(
        matches!(result, Err(FunctionCallError::RespondToModel(message)) if message.contains("not allowed in Plan mode"))
    );
    assert_eq!(session.services.plan_store.current_for_test().await, before);
}

#[test]
fn unchanged_plan_output_remains_compact() {
    let output = PlanToolOutput {
        lineage: Default::default(),
        current_plan: UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        },
        effect: PlanUpdateEffect::NoOp,
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
    assert_eq!(response["step_ids"], serde_json::json!([]));
    assert_eq!(response["revision"], crate::plan_store::plan_revision(Some(&output.current_plan)));
    assert_eq!(response["completion_authority"], "checklist_only");
    assert_eq!(response["obligations"], serde_json::json!({"completed":0,"superseded":0,"unresolved":[]}));
    assert_eq!(response["lineage_complete"], false);
    assert_eq!(response.as_object().expect("object response").len(), 9);
    assert_eq!(output.code_mode_result(&payload), response);
}

#[test]
fn update_plan_schema_is_the_simple_checklist_contract() {
    let tool = serde_json::to_value(create_update_plan_tool()).expect("serialize update_plan");
    assert!(
        tool["parameters"]["properties"]["plan"]["description"]
            .as_str()
            .unwrap()
            .contains("replacing the previous plan. Preserve the user's acceptance criteria")
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
        vec!["expected_revision", "expected_step_revisions", "explanation", "plan", "resolve", "scope_change_instructions", "set", "superseded", "workflow"]
    );
    assert_eq!(
        item_properties
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["continues", "status", "step"]
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
    assert!(validator.is_valid(&serde_json::json!({"expected_revision": "revision", "set": [{"index": 0, "status": "completed"}]})));
    assert!(validator.is_valid(&serde_json::json!({"set": [{"step_id": "stable-id", "status": "completed"}]})));
    assert!(validator.is_valid(&serde_json::json!({
        "plan": [{"step": "narrower", "status": "pending", "continues": ["old-id"]}],
        "superseded": [{"step_id": "dropped-id", "reason": "cancelled by the user"}],
    })));
    for invalid in [
        serde_json::json!({"plan": [], "superseded": [{"step_id": "dropped-id"}]}),
        serde_json::json!({"set": [{"status": "completed"}]}),
        serde_json::json!({"set": [{"index": 0, "step_id": "stable-id", "status": "completed"}]}),
        serde_json::json!({}),
        serde_json::json!({"plan": [], "investigation": {}}),
        serde_json::json!({"plan": [], "set": [{"index": 0, "status": "completed"}]}),
        serde_json::json!({"set": []}),
        serde_json::json!({"set": [{"index": -1, "status": "completed"}]}),
        serde_json::json!({"set": [{"index": 0, "status": "done"}]}),
    ] {
        assert!(!validator.is_valid(&invalid), "accepted {invalid}");
    }
    assert_eq!(
        tool.pointer("/parameters/properties/plan/items/required"),
        Some(&serde_json::json!(["status", "step"]))
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
        arguments: status_arguments("Implement the change", StepStatus::Completed),
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
    // Status-only updates still report the committed plan, so the turn
    // controller never retains a stale checklist.
    assert_eq!(
        completed
            .sampling_request_signal()
            .expect("status-only governor signal")["plan"],
        serde_json::json!(plan_update_args("Implement the change", StepStatus::Completed))
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
            "removed-investigation-field",
            serde_json::json!({"investigation": {}, "plan": []}).to_string(),
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
async fn plan_revision_must_carry_or_supersede_each_removed_unfinished_step() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let initial = UpdatePlanArgs {
        explanation: None,
        plan: vec![
            PlanItemArg {
                step: "Review every warning".to_string(),
                status: StepStatus::InProgress,
            },
            PlanItemArg {
                step: "Fix warnings".to_string(),
                status: StepStatus::Pending,
            },
        ],
    };
    session.services.plan_store.restore(Some(initial.clone())).await;
    let narrowed = serde_json::json!({
        "expected_revision": crate::plan_store::plan_revision(Some(&initial)),
        "explanation": "Only review core.",
        "plan": [{"step": "Review core warnings", "status": "in_progress"}],
    });
    let rejected = PlanHandler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "unaccounted-revision".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: narrowed.to_string(),
            },
        })
        .await;
    assert!(
        matches!(rejected, Err(FunctionCallError::RespondToModel(ref message)) if message.contains("Unaccounted"))
    );
    assert_eq!(
        session.services.plan_store.current_for_test().await,
        Some(initial)
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.msg, EventMsg::PlanUpdate(_)));
    }

    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "expected_revision": crate::plan_store::plan_revision(
                session.services.plan_store.snapshot().await.as_ref()),
            "plan": [{
                "step": "Review core warnings",
                "status": "in_progress",
                "continues": [crate::plan_store::plan_step_id("Review every warning")],
            }],
            "superseded": [{
                "step_id": crate::plan_store::plan_step_id("Fix warnings"),
                "reason": "The user will fix them separately.",
            }],
        })
        .to_string(),
    };
    let accepted = PlanHandler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(turn),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "accounted-revision".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("accounted revision");
    let output = accepted.code_mode_result(&payload);
    assert_plan_output_schema(&output);
    assert_eq!(output["effect"], "structural_revision");
    assert_eq!(
        output["current_plan"]["explanation"],
        "Proposed supersession (unverified) \"Fix warnings\": The user will fix them separately."
    );
    assert_eq!(
        accepted.sampling_request_signal().unwrap()["plan"],
        output["current_plan"]
    );
    let review_id = crate::plan_store::plan_step_id("Review every warning");
    let fix_id = crate::plan_store::plan_step_id("Fix warnings");
    let renamed_id = crate::plan_store::plan_step_id("Review core warnings");
    assert_eq!(output["lineage"]["requirements"][&review_id]["text"], "Review every warning");
    assert_eq!(output["lineage"]["step_identities"][&renamed_id], review_id);
    let mut carried = vec![review_id.clone(), renamed_id];
    carried.sort();
    assert_eq!(output["lineage"]["step_requirements"][&review_id], serde_json::json!(carried));
    assert_eq!(
        output["lineage"]["requirements"][&fix_id]["superseded_reason"],
        "The user will fix them separately."
    );
    let completed = session.services.plan_store.update_tool(serde_json::from_value(
        serde_json::json!({
            "expected_revision": output["revision"],
            "explanation": "Review complete.",
            "set": [{"step_id": review_id, "status": "completed"}],
        }),
    ).unwrap()).await.unwrap();
    assert_eq!(completed.lineage.requirements[&review_id].status, StepStatus::InProgress);
    let summary = completed.lineage.obligation_summary(&completed.current);
    assert_eq!(summary.completed, 1, "the descendant is complete, not its original scope");
    assert_eq!(summary.superseded, 0);
    assert!(summary.unresolved.contains(&review_id));
    assert!(summary.unresolved.contains(&fix_id));
    assert_eq!(completed.lineage.requirements[&fix_id].status, StepStatus::Pending);
    assert_eq!(
        completed.lineage.requirements[&fix_id].superseded_reason.as_deref(),
        Some("The user will fix them separately."),
        "replacing the explanation must not erase a superseded requirement"
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
    let revision = crate::plan_store::plan_revision(Some(&initial));
    for (arguments, has_plan, cancelled, error) in [
        (
            serde_json::json!({"set": [{"index": 0, "status": "completed"}]}),
            false,
            false,
            "create a plan",
        ),
        (serde_json::json!({"set": []}), true, false, "at least one"),
        (
            serde_json::json!({"set": [{"index": 0, "status": "completed"}]}),
            true,
            false,
            "require expected_revision",
        ),
        (
            serde_json::json!({"expected_revision": revision, "set": [{"index": 0, "status": "completed"}, {"index": 2, "status": "pending"}]}),
            true,
            false,
            "out of range",
        ),
        (
            serde_json::json!({"expected_revision": revision, "set": [{"index": 0, "status": "completed"}, {"index": 0, "status": "pending"}]}),
            true,
            false,
            "duplicate",
        ),
        (
            serde_json::json!({"expected_revision": revision, "set": [{"index": 1, "status": "in_progress"}]}),
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
    let mut expected = initial;
    expected.plan[0].status = StepStatus::Completed;
    expected.plan[1].status = StepStatus::InProgress;
    for effect in ["status_only", "no_op"] {
        let revision = session.services.plan_store.execution_snapshot().await.unwrap().revision;
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"expected_revision": revision, "set": [{"index": 0, "status": "completed"}, {"index": 1, "status": "in_progress"}]}).to_string(),
        };
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
        assert_eq!(
            result.sampling_request_signal().unwrap()["plan"],
            serde_json::json!(expected)
        );
        assert_eq!(
            session.services.plan_store.current_for_test().await,
            Some(expected.clone())
        );
        if effect == "no_op" {
            assert!(events.try_recv().is_err(), "published no-op must not emit another event");
        } else {
            assert!(
                matches!(events.recv().await.unwrap().msg, EventMsg::PlanUpdate(ref plan) if plan == &expected)
            );
        }
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
    let arguments = serde_json::json!({"set": [{
        "step_id": crate::plan_store::plan_step_id("Restore the accepted checklist"),
        "status": "completed",
    }]})
    .to_string();
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
