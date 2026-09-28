use super::*;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::handlers::ExecCommandHandler;
use crate::tools::handlers::WriteStdinHandler;
use codex_tools::ToolExecutor;
use serde_json::json;
use std::time::Instant;

fn report_projection(output: &dyn ToolOutput, payload: &ToolPayload, omitted: bool) -> JsonValue {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            BENCH_REPORT_OMISSION.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(BENCH_REPORT_OMISSION.with(|value| value.replace(Some(omitted))));
    output.code_mode_result(payload)
}

fn stats(samples: &[f64]) -> JsonValue {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!({"n": sorted.len(), "median_ms": sorted[sorted.len()/2],
        "p95_ms": sorted[((sorted.len() * 95).div_ceil(100) - 1).min(sorted.len()-1)],
        "samples_ms": samples})
}

fn render(output: &ExecCommandToolOutput, payload: &ToolPayload) -> JsonValue {
    json!({
        "direct": output.response_text(),
        "nested": output.code_mode_result(payload),
        "hook": output.post_tool_use_response("bench", payload),
        "canonical": output.canonical_result(payload),
    })
}

async fn fixture(
    raw: Vec<u8>,
    budget: usize,
    exit_code: i32,
) -> (ExecCommandToolOutput, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let artifact = create_raw_output_artifact(root.path(), "bench", &raw).await;
    assert!(matches!(artifact, RawOutputArtifact::Stored { .. }));
    (
        ExecCommandToolOutput {
            validation: None,
            event_call_id: "bench".into(),
            chunk_id: "bench".into(),
            wall_time: Duration::ZERO,
            raw_output: raw,
            truncation_policy: TruncationPolicy::Tokens(10_000),
            max_output_tokens: Some(budget),
            process_id: None,
            session_capabilities: None,
            exit_code: Some(exit_code),
            process_exited: true,
            search_no_match: false,
            original_token_count: None,
            hook_command: Some("custom-benchmark-command".into()),
            raw_output_artifact: Some(artifact),
            raw_output_truncated: false,
            raw_output_reduction_notice: None,
            repair_notice: None,
            pending_deferred_completions: Vec::new(),
        },
        root,
    )
}

#[tokio::test]
#[ignore = "explicit paired unified-exec benchmark; writes measurements under target"]
#[expect(clippy::print_stdout, reason = "emits the opt-in benchmark report")]
async fn unified_exec_targeted_benchmarks() {
    let payload = ToolPayload::Function {
        arguments: "{}".into(),
    };
    let mut measurements = Vec::new();
    for (name, raw, budget, exit_code) in [
        ("small_inline", b"complete output\n".to_vec(), 2000, 0),
        (
            "64k_success",
            b"source line with meaningful content\n".repeat(2000),
            2000,
            0,
        ),
        (
            "1m_failure",
            b"error: meaningful failure evidence\n".repeat(30_000),
            2000,
            1,
        ),
        (
            "zero_budget",
            b"evidence must remain recoverable\n".repeat(2000),
            0,
            0,
        ),
    ] {
        let (original, _root) = fixture(raw, budget, exit_code).await;
        let expected = render(&original, &payload);
        let mut baseline = Vec::new();
        let mut candidate = Vec::new();
        let mut saved = Vec::new();
        // Alternate order to avoid assigning all cache warmup or drift to one arm.
        // Fixture creation and cloning are outside the timed return path.
        for pair in 0..25 {
            let mut times = [0.0; 2];
            for arm in if pair % 2 == 0 { [0, 1] } else { [1, 0] } {
                let mut output = original.clone();
                let start = Instant::now();
                if arm == 0 {
                    output.prepare_reduction_notice().await;
                    output.prepare_reduction_notice().await;
                }
                let visible = std::hint::black_box(render(&output, &payload));
                times[arm] = start.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(
                    visible, expected,
                    "{name}: the removed work cannot change any consumer"
                );
            }
            if pair >= 4 {
                baseline.push(times[0]);
                candidate.push(times[1]);
                saved.push(times[0] - times[1]);
            }
        }
        let RawOutputArtifact::Stored { path, .. } = original.raw_output_artifact.as_ref().unwrap()
        else {
            unreachable!()
        };
        assert_eq!(tokio::fs::read(path).await.unwrap(), original.raw_output);
        measurements.push(json!({"case": name, "raw_bytes": original.raw_output.len(),
            "baseline": stats(&baseline), "candidate": stats(&candidate), "paired_saved": stats(&saved),
            "consumer_equality": true, "artifact_bytes_exact": true}));
    }

    // Negative controls reject a candidate that merely turns every reduction flag off.
    let (budget_limited, _root) = fixture(b"retained evidence\n".repeat(500), 4, 1).await;
    assert_eq!(
        report_projection(&budget_limited, &payload, false)["output_reduced"],
        true
    );
    let mut buffer = crate::unified_exec::head_tail_buffer::HeadTailBuffer::new(8);
    buffer.push_chunk(b"pass---word");
    let omitted = buffer.omitted_bytes() > 0;
    let (retention_limited, _root) = fixture(buffer.to_bytes_with_loss_notice(&[]), 2000, 0).await;
    assert!(omitted);
    assert_eq!(
        report_projection(&retention_limited, &payload, omitted)["output_reduced"],
        true
    );
    assert!(
        !report_projection(&retention_limited, &payload, omitted)["output"]
            .as_str()
            .unwrap()
            .contains("password")
    );

    let bursts = benchmark_real_bursts().await;
    let report = json!({"profile": if cfg!(debug_assertions) { "debug" } else { "optimized" },
        "scope": "local return-path A/B, not model-turn latency; candidate projection uses test-only per-report omission override",
        "preparation": measurements, "two_burst": bursts,
        "negative_controls": {"real_projection_truncation": true, "retention_gap": true}});
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/unified-exec-targeted-bench");
    tokio::fs::create_dir_all(&root).await.unwrap();
    tokio::fs::write(
        root.join("results.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .await
    .unwrap();
    println!("BENCH_REPORT={report}");
}

async fn benchmark_real_bursts() -> JsonValue {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    turn.approval_policy
        .set(codex_protocol::protocol::AskForApproval::Never)
        .unwrap();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let workspace = tempfile::tempdir().unwrap();
    let python = which::which("python")
        .or_else(|_| which::which("python3"))
        .unwrap();
    let mut baseline_warnings = 0;
    let mut candidate_warnings = 0;
    let mut times = Vec::new();
    for trial in 0..5 {
        let gate = workspace.path().join(format!("gate-{trial}"));
        let script = "import sys,time,pathlib\nsys.stdout.buffer.write(b'A'*5000);sys.stdout.buffer.flush()\nwhile not pathlib.Path(sys.argv[1]).exists(): time.sleep(0.005)\nsys.stdout.buffer.write(b'TAIL\\n');sys.stdout.buffer.flush()";
        let arguments = json!({"kind":"argv", "program": python, "args":["-u","-c",script,gate],
            "tty":false,"yield_time_ms":250,"max_output_tokens":2000})
        .to_string();
        let payload = ToolPayload::Function { arguments };
        let make_invocation = |name: &str, payload: ToolPayload| ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: format!("{name}-{trial}"),
            tool_name: ToolName::plain(name),
            cancellation_token: CancellationToken::new(),
            source: ToolCallSource::Direct,
            payload,
        };
        let opened = ExecCommandHandler::default()
            .handle(make_invocation("exec_command", payload.clone()))
            .await
            .unwrap();
        let head = opened.code_mode_result(&payload);
        assert_eq!(
            head["output"],
            "A".repeat(5000),
            "first call must actually deliver the entire burst"
        );
        assert_eq!(head["output_reduced"], false);
        let id = head["session_id"].as_u64().unwrap();
        tokio::fs::write(&gate, b"release").await.unwrap();
        let payload = ToolPayload::Function {
            arguments: json!({"session_id":id,"yield_time_ms":30000,"max_output_tokens":2000})
                .to_string(),
        };
        let start = Instant::now();
        let finished = WriteStdinHandler::default()
            .handle(make_invocation("write_stdin", payload.clone()))
            .await
            .unwrap();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        let baseline = finished.code_mode_result(&payload);
        let candidate = report_projection(finished.as_ref(), &payload, false);
        assert_eq!(baseline["output"], "TAIL\n");
        assert_eq!(baseline["exit_code"], 0);
        assert!(baseline.get("session_id").is_none());
        assert_eq!(
            baseline["output_reduced"], false,
            "earlier polls must not count as omitted output"
        );
        assert_eq!(candidate["output_reduced"], false);
        assert_eq!(candidate["output_complete"], true);
        let mut normalized = baseline.clone();
        normalized["output_reduced"] = json!(false);
        normalized["output_complete"] = json!(true);
        assert_eq!(
            candidate, normalized,
            "candidate must preserve output, artifact locator and status"
        );
        let artifact = turn
            .config
            .codex_home
            .join("tool-output")
            .join(session.thread_id.to_string())
            .join(format!(
                "{}.log",
                baseline["raw_output_artifact_id"].as_str().unwrap()
            ));
        let retained = tokio::fs::read(artifact).await.unwrap();
        assert_eq!(retained, format!("{}TAIL\n", "A".repeat(5000)).as_bytes());
        baseline_warnings += usize::from(baseline["output_reduced"] == true);
        candidate_warnings += usize::from(candidate["output_reduced"] == true);
    }
    assert!(session.list_background_terminals().await.is_empty());
    json!({"trials":5,"baseline_false_recovery_signals":baseline_warnings,
        "candidate_false_recovery_signals":candidate_warnings,"bytes_per_trial":5005,
        "baseline_final_poll":stats(&times), "extra_model_calls_measured":false,
        "complete_output_and_artifact_preserved":true})
}

#[tokio::test]
async fn unified_exec_reports_only_current_poll_omissions() {
    let report = benchmark_real_bursts().await;
    assert_eq!(report["baseline_false_recovery_signals"], 0);
    let payload = ToolPayload::Function {
        arguments: "{}".into(),
    };
    let (mut output, _home) = fixture(b"retained head and tail".to_vec(), 2000, 0).await;
    output.raw_output_truncated = true;
    let result = output.code_mode_result(&payload);
    assert_eq!(result["output_reduced"], true);
    assert_eq!(result["output_complete"], false);
}
