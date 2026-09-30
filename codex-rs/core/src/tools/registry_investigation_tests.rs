use super::*;
use crate::plan_store::investigation::Finding;
use crate::plan_store::investigation::FindingKind;
use crate::plan_store::investigation::HypothesisStatus;
use crate::plan_store::investigation::Phase;
use crate::session::step_context::StepContext;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::PlanHandler;
use crate::turn_diff_tracker::TurnDiffTracker;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

struct Probe {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    fails_to_execute: bool,
}

impl ToolExecutor<ToolInvocation> for Probe {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.name)
    }

    fn spec(&self) -> ToolSpec {
        crate::tools::handlers::plan_spec::create_update_plan_tool()
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fails_to_execute {
                return Err(FunctionCallError::RespondToModel(
                    "sandbox could not execute probe".into(),
                ));
            }
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                "observed matching live completion and reducer retaining inProgress".into(),
                Some(true),
            )))
        })
    }
}
impl CoreToolRuntime for Probe {}

#[tokio::test]
async fn investigation_dispatch_gates_direct_and_nested_edits_and_records_real_observations() {
    for nested in [false, true] {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut invocation = ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::new(turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "investigation-plan".into(),
            tool_name: ToolName::plain("update_plan"),
            source: if nested {
                ToolCallSource::CodeMode {
                    cell_id: "cell".into(),
                    parent_call_id: Some("outer".into()),
                    runtime_tool_call_id: "nested".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                }
            } else {
                ToolCallSource::Direct
            },
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "plan":[],
                    "investigation":crate::plan_store::investigation::tests::report(),
                })
                .to_string(),
            },
        };
        let result = handle_any_tool(&PlanHandler, invocation.clone())
            .await
            .unwrap();
        let response = result.result.code_mode_result(&invocation.payload);
        assert_eq!(
            response["current_plan"]["investigation"]["phase"],
            "investigating"
        );
        let schema = crate::tools::handlers::plan_spec::create_update_plan_tool();
        let ToolSpec::Function(schema) = schema else {
            panic!("function schema")
        };
        assert!(
            jsonschema::validator_for(&schema.output_schema.unwrap().to_value())
                .unwrap()
                .is_valid(&response)
        );
        invocation.tool_name = ToolName::plain("apply_patch");
        invocation.payload = ToolPayload::Custom {
            input: "production patch".into(),
        };
        let edit = Probe {
            name: "apply_patch",
            calls: Arc::clone(&calls),
            fails_to_execute: false,
        };
        assert!(handle_any_tool(&edit, invocation.clone()).await.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "blocked before handler execution"
        );

        invocation.tool_name = ToolName::plain("read_file");
        invocation.payload = ToolPayload::Function {
            arguments: "{}".into(),
        };
        let failed = Probe {
            name: "read_file",
            calls: Arc::clone(&calls),
            fails_to_execute: true,
        };
        assert!(handle_any_tool(&failed, invocation.clone()).await.is_err());
        let mut report = crate::plan_store::investigation::tests::report();
        report.hypotheses[0].status = HypothesisStatus::RuledOut;
        report.hypotheses[1].status = HypothesisStatus::Established;
        report.phase = Phase::Implementing;
        report.unknowns.clear();
        report.finding = Some(Finding {
            kind: FindingKind::CauseEstablished,
            observation_id: "boundary".into(),
            hypothesis_ids: vec!["backend".into(), "desktop".into()],
            uncertainty: "which side loses completion".into(),
            evidence: "matching completion reaches receiver; reducer keeps inProgress".into(),
            conclusion: "receiver drops the terminal transition".into(),
        });
        assert!(
            session
                .services
                .plan_store
                .update_tool(Some(Vec::new()), None, None, Some(report.clone()))
                .await
                .is_err()
        );
        let read = Probe {
            name: "read_file",
            calls: Arc::clone(&calls),
            fails_to_execute: false,
        };
        handle_any_tool(&read, invocation.clone()).await.unwrap();
        session
            .services
            .plan_store
            .update_tool(Some(Vec::new()), None, None, Some(report))
            .await
            .unwrap();
        invocation.tool_name = ToolName::plain("apply_patch");
        invocation.payload = ToolPayload::Custom {
            input: "production patch".into(),
        };
        handle_any_tool(&edit, invocation).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
