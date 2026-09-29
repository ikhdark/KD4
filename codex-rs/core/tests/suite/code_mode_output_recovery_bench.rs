//! Full-turn benchmark with deterministic mock responses, not live inference.
use super::assert_eq;
use super::*;
use std::io::Write;
use std::time::Instant;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in complete-turn output/recovery benchmark"]
async fn full_turn_output_recovery_benchmark() -> Result<()> {
    require_network!();
    for sample in 0..3 {
        for candidate in if sample % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let server = responses::start_mock_server().await;
            let mut builder = test_codex().with_config(|config| {
                let _ = config.features.enable(Feature::CodeMode);
            });
            let test = builder.build(&server).await?;
            let source = (0..3_000).map(|i| format!("ROW_{i:04}: exact source evidence for operation {i}; preserve this result.\n")).collect::<String>();
            std::fs::write(test.cwd.path().join("evidence.txt"), &source)?;
            let prefix = if candidate {
                "// @exec: {\"max_output_tokens\": 10000}\n"
            } else {
                ""
            };
            let code = format!(
                "{prefix}const r = await tools.read_file({{path:'evidence.txt'}}); if (!r.artifact_id) throw new Error('expected retained source'); store('evidence-id',r.artifact_id); text(r.results[0].text);"
            );
            responses::mount_sse_once(
                &server,
                sse(vec![
                    ev_response_created("initial-response"),
                    ev_custom_tool_call("initial", "exec", &code),
                    ev_completed("initial-response"),
                ]),
            )
            .await;
            let second = if candidate {
                responses::mount_sse_once(
                    &server,
                    sse(vec![
                        ev_assistant_message("answer", "ROW_0150 verified"),
                        ev_completed("done"),
                    ]),
                )
                .await
            } else {
                responses::mount_sse_once(&server, sse(vec![
                    ev_custom_tool_call("recover", "exec", "const r = await tools.read_tool_output({artifact_id:load('evidence-id'),selectors:[{kind:'search',query:'ROW_0150',max_results:1}]}); text(r.results[0].value.hydrated_ranges[0].text);"),
                    ev_completed("recovery-response"),
                ])).await
            };
            let final_response = if candidate {
                None
            } else {
                Some(
                    responses::mount_sse_once(
                        &server,
                        sse(vec![
                            ev_assistant_message("answer", "ROW_0150 verified"),
                            ev_completed("done"),
                        ]),
                    )
                    .await,
                )
            };
            // Excludes fixture creation and file setup. Includes model transport,
            // tool dispatch, code-mode host activity, recovery and final completion.
            let started = Instant::now();
            test.submit_turn("Read evidence.txt and verify the exact ROW_0150 record.")
                .await?;
            let elapsed = started.elapsed();
            let first_visible =
                custom_tool_output_last_non_empty_text(&second.single_request(), "initial")
                    .expect("initial output");
            assert_eq!(
                first_visible.contains("ROW_0150"),
                candidate,
                "fixture must require recovery only in baseline"
            );
            if let Some(final_response) = final_response {
                let exact = custom_tool_output_last_non_empty_text(
                    &final_response.single_request(),
                    "recover",
                )
                .expect("recovered output");
                assert_eq!(exact.trim_end(), source.lines().nth(150).unwrap());
            }
            let model_requests = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path().contains("responses"))
                .count();
            assert_eq!(model_requests, if candidate { 2 } else { 3 });
            let record = serde_json::json!({"sample":sample,"candidate":candidate,
                "complete_turn_ns":elapsed.as_nanos() as u64,"model_requests":model_requests,
                "recovery_calls":usize::from(!candidate),"task_success":true,
                "scope":"real core turn/code-mode/file/recovery with scripted mock model; existing explicit 10000-token pragma, not production default change"});
            if let Some(path) = std::env::var_os("OUTPUT_RECOVERY_E2E_OUTPUT") {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;
                writeln!(file, "{record}")?;
            }
        }
    }
    Ok(())
}
