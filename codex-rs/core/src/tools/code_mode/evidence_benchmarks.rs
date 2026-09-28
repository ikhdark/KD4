//! Production-path regressions converted from the four manual A/B probes.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputOutcome;
use serde_json::Value;
use serde_json::json;

use super::PacketRuntime;
use super::PacketTestTool;
use super::nested_result_evidence;
use super::packet_output_text;
use crate::session::turn_execution::SamplingRequestSettledState;
use crate::session::turn_execution::SamplingRequestSignalCollector;
use crate::session::turn_execution::TurnExecutionControl;
use crate::tools::context::ToolPayload;

fn record_outer(control: &mut TurnExecutionControl, output: &dyn ToolOutput, source: &str) -> bool {
    let baselines = control.baselines(0);
    let collector = SamplingRequestSignalCollector::default();
    let payload = ToolPayload::Custom {
        input: source.into(),
    };
    let registration = collector.register_deterministic_tool_call(
        &ToolName::plain("exec"),
        &payload,
        "regression-outer",
    );
    collector.record_response_result(
        registration.ordinal,
        output.outcome_context(),
        output.sampling_request_signal(),
        &output.to_response_item("regression-outer", &payload),
        false,
    );
    control.observe_budget_progress(
        &baselines,
        &collector,
        &SamplingRequestSettledState {
            mutation_revision: 0,
            tool_exposure_revision: 0,
        },
    )
}

#[tokio::test]
async fn failure_identity_does_not_renew_progress_for_new_cells() {
    let runtime = PacketRuntime::new().await;
    let source = "throw new Error('SAME_DIAGNOSTIC');";
    let mut control = TurnExecutionControl::new();
    let mut signatures = BTreeSet::new();
    let mut cells = BTreeSet::new();
    for index in 0..3 {
        let output = runtime.exec(source).await;
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
        assert!(packet_output_text(output.as_ref()).contains("SAME_DIAGNOSTIC"));
        cells.insert(output.deterministic_continuation_owner_key().unwrap());
        let signal = output.sampling_request_signal().unwrap();
        assert_eq!(signal["semantic_evidence"]["status"], "failed");
        signatures.insert(signal["failure_signature"].as_str().unwrap().to_owned());
        assert_eq!(
            record_outer(&mut control, output.as_ref(), source),
            index == 0
        );
    }
    assert_eq!(cells.len(), 3);
    assert_eq!(signatures.len(), 1);
    let changed_source = "throw new Error('CHANGED_DIAGNOSTIC');";
    let changed = runtime.exec(changed_source).await;
    let signal = changed.sampling_request_signal().unwrap();
    assert!(!signatures.contains(signal["failure_signature"].as_str().unwrap()));
    assert!(record_outer(&mut control, changed.as_ref(), changed_source));
    runtime.finish().await;
}

#[test]
fn duplicate_detection_keeps_changed_middles_and_exit_statuses() {
    let first = format!("{}FIRST_MIDDLE{}", "H".repeat(300), "T".repeat(300));
    let changed = format!("{}UNPRINTED_DIAGNOSTIC{}", "H".repeat(300), "T".repeat(300));
    let emitted = json!({"exit_code":0,"output":first}).to_string();
    for (name, exit, payload, duplicate) in [
        ("identical", 0, first.as_str(), true),
        ("changed_middle", 0, changed.as_str(), false),
        ("changed_exit", 7, first.as_str(), false),
    ] {
        let result = super::CodeModeNestedResultEvidence {
            failed: exit != 0,
            ..nested_result_evidence(&format!("exit_code: {exit}\n{payload}"))
        };
        assert_eq!(
            super::super::nested_result_already_emitted(&result, &emitted),
            duplicate,
            "{name}"
        );
        assert!(!super::super::nested_result_already_emitted(
            &super::CodeModeNestedResultEvidence {
                output_truncated: true,
                ..result.clone()
            },
            &emitted,
        ));
        let formatted = super::format_runtime_response(
            codex_code_mode::RuntimeResponse::Result {
                cell_id: codex_code_mode::CellId::new("dedup-regression".into()),
                content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText {
                    text: emitted.clone(),
                }],
                error_text: Some("later script failure".into()),
                output_loss: None,
            },
            None,
            10_000,
            true,
            Instant::now(),
            Vec::new(),
            vec![result],
            None,
        );
        let visible = packet_output_text(&formatted);
        assert_eq!(formatted.outcome_for_logging(), ToolOutputOutcome::Failure);
        assert_eq!(visible.matches("FIRST_MIDDLE").count(), 1);
        assert_eq!(
            visible.contains("UNPRINTED_DIAGNOSTIC"),
            name == "changed_middle"
        );
        assert_eq!(visible.contains("exit_code: 7"), name == "changed_exit");
    }
}

fn recovery_receipt(output: &dyn ToolOutput) -> Value {
    packet_output_text(output)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value["output_truncated"] == true)
        .expect("clipped fallback must expose structured recovery metadata")
}

#[tokio::test]
async fn clipped_command_fallback_reuses_raw_artifact_even_at_zero_budget() {
    let runtime = PacketRuntime::with_tools(
        vec![Arc::new(
            crate::tools::handlers::ExecCommandHandler::default(),
        )],
        |turn| {
            turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
            turn.approval_policy
                .set(codex_protocol::protocol::AskForApproval::Never)
                .unwrap();
        },
    )
    .await;
    let fixture = tempfile::tempdir().unwrap();
    let data = format!(
        "{}OMITTED_MIDDLE_SENTINEL{}END",
        "H".repeat(3000),
        "T".repeat(3000)
    );
    for budget in [0, 10_000] {
        let source_path = fixture.path().join(format!("source-{budget}.txt"));
        std::fs::write(&source_path, &data).unwrap();
        let command = if cfg!(windows) {
            format!(
                "Get-Content -Raw -LiteralPath '{}'",
                source_path.display().to_string().replace('\'', "''")
            )
        } else {
            format!("cat '{}'", source_path.display())
        };
        let args = json!({"cmd":command,"workdir":fixture.path(),"yield_time_ms":30_000,"max_output_tokens":8_000});
        let output = runtime.exec(&format!(
            "// @exec: {{\"max_output_tokens\":{budget}}}\nconst r = await tools.exec_command({args}); if (r.session_id != null || r.exit_code !== 0 || r.output_reduced) throw new Error('fixture must finish without nested truncation'); store('fallback_result', r);"
        )).await;
        assert_eq!(
            output.outcome_for_logging(),
            ToolOutputOutcome::Success,
            "{}",
            packet_output_text(output.as_ref())
        );
        assert!(!packet_output_text(output.as_ref()).contains("OMITTED_MIDDLE_SENTINEL"));
        let receipt = recovery_receipt(output.as_ref());
        let artifact_id = receipt["artifact_id"].as_str().unwrap();
        let captured = runtime.exec("text(load('fallback_result'));").await;
        let raw: Value = serde_json::from_str(&packet_output_text(captured.as_ref())).unwrap();
        assert_eq!(
            raw["raw_output_artifact_id"], artifact_id,
            "reuse the command's artifact"
        );
        assert_eq!(raw["output_reduced"], false);
        assert_eq!(raw["output"].as_str().unwrap().trim_end(), data);
        std::fs::rename(
            &source_path,
            fixture.path().join(format!("unavailable-{budget}.txt")),
        )
        .unwrap();
        assert!(!source_path.exists());
        let recovered = crate::tools::command_output_artifact::read_exact_tool_output_artifact(
            &runtime.step.turn.config.codex_home,
            &runtime.session.thread_id.to_string(),
            artifact_id,
        )
        .await
        .unwrap();
        assert_eq!(recovered, raw["output"].as_str().unwrap().as_bytes());
    }
    runtime.finish().await;
}

#[tokio::test]
async fn clipped_generic_result_and_caught_error_keep_complete_snapshots() {
    let runtime = PacketRuntime::new().await;
    let small = runtime.exec("await tools.read_tool_output({});").await;
    assert!(!packet_output_text(small.as_ref()).contains("artifact_id"));
    let data = format!("{}GENERIC_MIDDLE{}END", "é".repeat(3000), "界".repeat(3000));
    for (field, script) in [
        (
            "text",
            "const r = await tools.read_tool_output(ARGS); store('generic_result', r);",
        ),
        (
            "error",
            "try { await tools.read_tool_output(ARGS); } catch (e) { store('generic_result', e.message); }",
        ),
    ] {
        let mut args = serde_json::Map::new();
        args.insert(field.to_string(), json!(data));
        let source = script.replace("ARGS", &Value::Object(args).to_string());
        let output = runtime.exec(&source).await;
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
        let receipt = recovery_receipt(output.as_ref());
        assert_eq!(receipt["recovery_tool"], "read_tool_output");
        assert_eq!(
            receipt["selectors"],
            json!([{"kind":"json_pointer","pointer":""}])
        );
        let bytes = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
            &runtime.step.turn.config.codex_home,
            &runtime.session.thread_id.to_string(),
            receipt["artifact_id"].as_str().unwrap(),
            100_000,
        )
        .await
        .unwrap();
        let recovered: Value = serde_json::from_slice(&bytes).unwrap();
        let captured = runtime.exec("text({result:load('generic_result')});").await;
        let captured: Value = serde_json::from_str(&packet_output_text(captured.as_ref())).unwrap();
        assert_eq!(recovered, captured["result"]);
        assert!(recovered.to_string().contains("GENERIC_MIDDLE"));
    }
    runtime.finish().await;
}

#[tokio::test]
async fn failed_snapshot_is_reported_without_a_false_recovery_handle() {
    let fixture = tempfile::tempdir().unwrap();
    let not_a_directory = fixture.path().join("not-a-directory");
    std::fs::write(&not_a_directory, "file").unwrap();
    let runtime = PacketRuntime::with_tools(
        vec![Arc::new(PacketTestTool {
            name: "read_tool_output",
        })],
        |turn| {
            Arc::make_mut(&mut turn.config).codex_home =
                codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(not_a_directory)
                    .unwrap();
        },
    )
    .await;
    let args = json!({"text":"x".repeat(6000)});
    let output = runtime
        .exec(&format!("await tools.read_tool_output({args});"))
        .await;
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
    let receipt = recovery_receipt(output.as_ref());
    assert_eq!(receipt["recovery_unavailable"], true);
    assert!(
        receipt["recovery_error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    assert!(receipt.get("artifact_id").is_none());
    runtime.finish().await;
}

#[tokio::test]
async fn existing_snapshot_is_reused_only_for_identical_complete_results() {
    let runtime = PacketRuntime::new().await;
    let exec = super::super::ExecContext {
        session: Arc::clone(&runtime.session),
        turn: Arc::clone(&runtime.step.turn),
    };
    let first = json!({"text":"original"});
    let receipt = super::super::nested_result_snapshot(&exec, "first", &first, None).await;
    let canonical = codex_tools::CanonicalToolResult::json(first.clone());
    let identity = (
        receipt["artifact_id"].as_str().unwrap().to_string(),
        canonical.sha256,
    );
    let reused = super::super::nested_result_snapshot(&exec, "same", &first, Some(&identity)).await;
    assert_eq!(reused["artifact_id"], receipt["artifact_id"]);
    let changed = json!({"text":"changed"});
    let different =
        super::super::nested_result_snapshot(&exec, "different", &changed, Some(&identity)).await;
    assert_ne!(different["artifact_id"], receipt["artifact_id"]);
    let bytes = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
        &runtime.step.turn.config.codex_home,
        &runtime.session.thread_id.to_string(),
        different["artifact_id"].as_str().unwrap(),
        1000,
    )
    .await
    .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), changed);
    runtime.finish().await;
}

struct GateTool {
    name: &'static str,
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Arc<tokio::sync::Notify>,
}

impl codex_tools::ToolExecutor<crate::tools::context::ToolInvocation> for GateTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.name)
    }
    fn spec(&self) -> codex_tools::ToolSpec {
        codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: self.name.into(),
            description: "Deterministic regression gate.".into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }
    fn handle(
        &self,
        invocation: crate::tools::context::ToolInvocation,
    ) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            tokio::select! {
                _ = self.release.notified() => {},
                _ = invocation.cancellation_token.cancelled() => return Err(crate::FunctionCallError::RespondToModel("regression gate cancelled".into())),
            }
            Ok(crate::tools::context::boxed_tool_output(
                crate::tools::context::FunctionToolOutput::from_text("released".into(), Some(true)),
            ))
        })
    }
}

impl crate::tools::registry::CoreToolRuntime for GateTool {}

#[tokio::test]
async fn interrupted_exec_and_wait_deliver_pending_results_exactly_once() {
    for resume_wait in [false, true] {
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let (start_entered, start_entered_rx) = tokio::sync::oneshot::channel();
        let start_release = Arc::new(tokio::sync::Notify::new());
        let runtime = PacketRuntime::with_tools(
            vec![
                Arc::new(PacketTestTool {
                    name: "read_tool_output",
                }),
                Arc::new(GateTool {
                    name: "benchmark_gate",
                    entered: std::sync::Mutex::new(Some(entered)),
                    release: Arc::clone(&release),
                }),
                Arc::new(GateTool {
                    name: "start_gate",
                    entered: std::sync::Mutex::new(Some(start_entered)),
                    release: Arc::clone(&start_release),
                }),
            ],
            |_| {},
        )
        .await;
        *runtime.session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
        let source = "await tools.read_tool_output({}); await tools.benchmark_gate({}); throw new Error('after steering');";
        let wait_cell = if resume_wait {
            let initial = runtime
                .exec(&format!(
                    "await yield_control(); await tools.start_gate({{}}); {source}"
                ))
                .await;
            assert_eq!(initial.outcome_for_logging(), ToolOutputOutcome::Yielded);
            assert!(!packet_output_text(initial.as_ref()).contains("READ_RESULT_42"));
            start_entered_rx.await.unwrap();
            start_release.notify_one();
            Some((runtime.live_cell(), packet_output_text(initial.as_ref())))
        } else {
            None
        };
        let (interrupted, ()) = tokio::join!(
            async {
                match &wait_cell {
                    Some((cell, _)) => runtime.wait(cell).await,
                    None => runtime.exec(source).await,
                }
            },
            async {
                entered_rx.await.unwrap();
                runtime
                    .session
                    .inject_if_running(vec![codex_protocol::models::ResponseItem::Message {
                        id: None,
                        role: "user".into(),
                        content: vec![codex_protocol::models::ContentItem::InputText {
                            text: "status?".into(),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    }])
                    .await
                    .unwrap();
            }
        );
        assert_eq!(
            interrupted.outcome_for_logging(),
            ToolOutputOutcome::Yielded
        );
        let interrupted_text = packet_output_text(interrupted.as_ref());
        assert!(
            interrupted_text.contains("new user input"),
            "{interrupted_text}"
        );
        let cell = runtime.live_cell();
        release.notify_one();
        let terminal = runtime.wait(&cell).await;
        assert_eq!(terminal.outcome_for_logging(), ToolOutputOutcome::Failure);
        let terminal_text = packet_output_text(terminal.as_ref());
        assert!(terminal_text.contains("after steering"));
        let all = format!(
            "{}\n{interrupted_text}\n{terminal_text}",
            wait_cell.map(|(_, text)| text).unwrap_or_default()
        );
        assert_eq!(all.matches("READ_RESULT_42").count(), 1, "{all}");
        assert!(
            runtime
                .session
                .services
                .code_mode_service
                .packet_admission
                .lock()
                .unwrap()
                .cells
                .is_empty()
        );
        runtime.finish().await;
    }
}
