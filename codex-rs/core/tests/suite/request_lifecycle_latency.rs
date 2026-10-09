use anyhow::Result;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;

/// Includes preparation, two physical requests, a real tool, and turn finalization.
/// Run explicitly with `--run-ignored only`; no live provider is involved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "wall-clock benchmark against a scripted loopback provider"]
async fn request_lifecycle_wall_clock_benchmark() -> Result<()> {
    core_test_support::require_network!();
    let mut samples = Vec::new();
    for prompt_bytes in [4 * 1024, 512 * 1024] {
        for sample in 0..8 {
            let server = responses::start_mock_server().await;
            let requests = responses::mount_sse_sequence(&server, vec![
                responses::sse(vec![
                    responses::ev_response_created("latency-tool"),
                    responses::ev_function_call("latency-plan", "update_plan", &serde_json::json!({
                        "plan": [{"step": "Measure the request lifecycle", "status": "completed"}]
                    }).to_string()),
                    responses::ev_completed("latency-tool"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("latency-answer", "latency complete"),
                    responses::ev_completed("latency-final"),
                ]),
            ]).await;
            let test = test_codex().with_config(|config| {
                config.features.disable(Feature::Apps).unwrap();
                config.features.disable(Feature::CodeModeHost).unwrap();
                config.model_context_window = Some(2_000_000);
                config.model_auto_compact_token_limit = Some(i64::MAX);
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(0);
            }).build(&server).await?;
            let prompt = "x".repeat(prompt_bytes);
            let start = std::time::Instant::now();
            let completed = test.submit_turn_and_capture_completion(&prompt).await?;
            let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
            assert!(completed.error.is_none(), "{completed:?}");
            assert_eq!(completed.last_agent_message.as_deref(), Some("latency complete"));
            assert_eq!(requests.requests().len(), 2);
            let output = requests.requests()[1].function_call_output_text("latency-plan")
                .expect("the real tool result must reach the continuation");
            let output: serde_json::Value = serde_json::from_str(&output)?;
            assert_eq!(output["message"], "Plan updated");
            assert_eq!(output["current_plan"]["plan"], serde_json::json!([
                {"step": "Measure the request lifecycle", "status": "completed"}
            ]));
            samples.push(serde_json::json!({
                "prompt_bytes": prompt_bytes, "sample": sample, "warmup": sample == 0,
                "wall_ms": wall_ms, "requests": 2, "tool_calls": 1,
                "exact_answer": true, "timing": completed.timing,
            }));
            test.codex.shutdown_and_wait().await?;
        }
    }
    if let Some(path) = std::env::var_os("KD4_REQUEST_LIFECYCLE_BENCHMARK_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&samples)?)?;
    }
    for sample in &samples {
        eprintln!("REQUEST_LIFECYCLE_BENCHMARK {}", serde_json::json!({
            "prompt_bytes": sample["prompt_bytes"], "sample": sample["sample"],
            "wall_ms": sample["wall_ms"], "requests": sample["requests"],
            "exact_answer": sample["exact_answer"],
        }));
    }
    Ok(())
}
