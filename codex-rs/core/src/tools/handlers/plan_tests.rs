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
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn enable_task_evidence(
    session: &mut Arc<crate::session::session::Session>,
) -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("task evidence tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("codex-home");
    tokio::fs::create_dir_all(repo.join("scripts"))
        .await
        .expect("task evidence scripts directory");
    tokio::fs::write(repo.join("scripts/verify_local.py"), "# fixture")
        .await
        .expect("task evidence verifier fixture");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("task evidence manifest fixture");
    let evidence_path = codex_home
        .join("task-evidence")
        .join(format!("{}.json", session.thread_id));
    let ledger =
        crate::task_evidence::TaskEvidenceLedger::load_or_new(codex_home, session.thread_id, &repo)
            .await;
    Arc::get_mut(session)
        .expect("single session reference")
        .services
        .task_evidence = ledger;
    (temp, evidence_path)
}

fn plan_arguments(step: &str) -> String {
    serde_json::to_string(&UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            id: Some("step".to_string()),
            step: step.to_string(),
            status: StepStatus::Pending,
            ..Default::default()
        }],
    })
    .expect("serialize plan arguments")
}

fn plan_update_args(id: Option<&str>, step: &str, status: StepStatus) -> UpdatePlanArgs {
    UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            id: id.map(str::to_string),
            step: step.to_string(),
            status,
            ..Default::default()
        }],
    }
}

async fn read_persisted_plan(evidence_path: &PathBuf) -> serde_json::Value {
    serde_json::from_slice(
        &tokio::fs::read(evidence_path)
            .await
            .expect("read persisted task evidence"),
    )
    .expect("parse persisted task evidence")
}

async fn invoke_normalized_plan_update(
    args: UpdatePlanArgs,
) -> (serde_json::Value, serde_json::Value) {
    let (mut session, turn, _events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    let arguments = serde_json::to_string(&args).expect("serialize plan update");
    let payload = ToolPayload::Function { arguments };
    let output = PlanHandler
        .handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "normalized-plan-output".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("plan update should succeed");

    let ResponseInputItem::FunctionCallOutput {
        output: response, ..
    } = output.to_response_item("normalized-plan-output", &payload)
    else {
        panic!("plan update should return function output");
    };
    let FunctionCallOutputBody::Text(text) = response.body else {
        panic!("plan update should return text output");
    };
    let ordinary_result =
        serde_json::from_str(&text).expect("normalized plan output should be JSON");
    let code_mode_result = output.code_mode_result(&payload);
    assert_eq!(ordinary_result, code_mode_result);

    (ordinary_result, read_persisted_plan(&evidence_path).await)
}

#[test]
fn unchanged_plan_output_remains_compact() {
    let output = PlanToolOutput {
        normalized_plan: None,
    };
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let ResponseInputItem::FunctionCallOutput {
        output: response, ..
    } = output.to_response_item("unchanged-plan-output", &payload)
    else {
        panic!("plan update should return function output");
    };

    assert_eq!(
        response.body.to_text().as_deref(),
        Some(PLAN_UPDATED_MESSAGE)
    );
    assert_eq!(output.code_mode_result(&payload), serde_json::json!({}));
}

#[tokio::test]
async fn normalized_plan_output_reports_downgraded_success_statuses() {
    for requested_status in [StepStatus::Passed, StepStatus::Completed] {
        let (result, _) = invoke_normalized_plan_update(plan_update_args(
            Some("step"),
            "Implement the step",
            requested_status,
        ))
        .await;

        assert_eq!(result["message"], PLAN_UPDATED_MESSAGE);
        assert_eq!(
            result["normalized_plan"]["plan"][0]["status"],
            "implemented"
        );
    }
}

#[tokio::test]
async fn normalized_plan_output_returns_the_installed_compatibility_id() {
    let (result, persisted) = invoke_normalized_plan_update(plan_update_args(
        None,
        "Install a stable identifier",
        StepStatus::Pending,
    ))
    .await;

    let returned_id = result["normalized_plan"]["plan"][0]["id"]
        .as_str()
        .expect("normalized plan should return a stable id");
    assert!(returned_id.starts_with("step-"));
    assert_eq!(persisted["plan"][0]["id"], returned_id);
}

#[test]
fn update_plan_waits_for_runtime_cancellation_commit_cleanup() {
    assert!(PlanHandler.waits_for_runtime_cancellation());
}

#[tokio::test]
async fn cancellation_before_plan_commit_does_not_emit_plan_update() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();
    let arguments = plan_arguments("Do not commit this plan");
    let invocation = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token,
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "cancelled-plan".to_string(),
        tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function { arguments },
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
    let evidence = read_persisted_plan(&evidence_path).await;
    assert_eq!(evidence["plan"], serde_json::json!([]));
}

#[tokio::test]
async fn cancellation_after_plan_commit_boundary_waits_for_durable_update() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    let call_id = "cancelled-after-plan-commit-boundary";
    let hook = PlanCommitBoundaryHook::install(call_id);
    let handler = Arc::new(PlanHandler) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
    ));
    let runtime = ToolCallRuntime::new(
        router,
        session,
        StepContext::for_test(Arc::clone(&turn)),
        Arc::new(Mutex::new(TurnDiffTracker::new())),
    );
    let cancellation_token = CancellationToken::new();
    let call = ToolCall {
        tool_name: ToolName::plain("update_plan"),
        call_id: call_id.to_string(),
        payload: ToolPayload::Function {
            arguments: plan_arguments("Commit this plan before returning cancellation"),
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
    let evidence = read_persisted_plan(&evidence_path).await;
    assert_eq!(
        evidence["plan"][0]["step"],
        "Commit this plan before returning cancellation"
    );
}
