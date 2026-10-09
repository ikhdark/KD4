//! Opt-in complete model/tool/model turns against a loopback scripted provider.
//! This is not a provider-inference benchmark or proof of a queue-fix speedup.
use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "end-to-end wall-clock scheduling benchmark"]
async fn model_tool_model_wall_clock_benchmark() -> Result<()> {
    require_network!();
    for (scenario, source, expected) in [
        ("timer-continuation", r#"// @exec: {"yield_time_ms": 5}
await new Promise(resolve => setTimeout(resolve, 40));
text("ready");"#, "ready"),
        ("parallel-tool-continuation", r#"
const args = {sleep_after_ms: 40, barrier: {
    id: "idle-wall-clock", participants: 2, timeout_ms: 5000
}};
text(JSON.stringify(await Promise.all([
    tools.test_sync_tool(args), tools.test_sync_tool(args)
])));"#, "[\"ok\",\"ok\"]"),
    ] {
        let mut samples = Vec::new();
        for round in 0..8 {
            let server = responses::start_mock_server().await;
            let mut builder = test_codex().with_model("test-gpt-5.1-codex")
                .with_config(|config| { let _ = config.features.enable(Feature::CodeMode); });
            let test = builder.build(&server).await?;
            let model = responses::mount_sse_sequence(&server, vec![
                sse(vec![ev_response_created("resp-1"),
                    ev_custom_tool_call("call-1", "exec", source), ev_completed("resp-1")]),
                sse(vec![ev_assistant_message("msg-1", "done"), ev_completed("resp-2")]),
            ]).await;
            let start = Instant::now();
            tokio::time::timeout(Duration::from_secs(30), test.submit_turn("Execute and report the result")).await??;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            let request = model.last_request().expect("tool result reached next model request");
            let items = custom_tool_output_items(&request, "call-1");
            assert_eq!(items.len(), 1);
            assert_eq!(text_item(&items, 0), expected);
            let requests = server.received_requests().await.unwrap().into_iter()
                .filter(|request| request.url.path().contains("responses")).count();
            assert_eq!(requests, 2, "no empty heartbeat or polling generations");
            test.codex.submit(Op::Shutdown).await?;
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    match test.codex.next_event().await.unwrap().msg {
                        EventMsg::ShutdownComplete => break,
                        EventMsg::Error(error) => panic!("shutdown failed: {}", error.message),
                        _ => {}
                    }
                }
            }).await?;
            if round > 0 { samples.push(elapsed_ms); }
        }
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        eprintln!("{}", serde_json::json!({
            "scenario": scenario, "samples_ms": samples,
            "median_ms": sorted[sorted.len()/2], "model_requests_per_turn": 2,
            "scope": "submit through real core, HTTP model streaming, code mode, result relay and final completion",
            "limitations": "scripted provider; excludes fixture startup, native shell, and real inference; not an A/B queue-fix measurement"
        }));
    }
    Ok(())
}
