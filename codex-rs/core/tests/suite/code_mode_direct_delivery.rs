//! Complete core turns; the provider is scripted, not live inference.
use super::assert_eq;
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_delivery_preserves_answer_and_removes_final_model_request() -> Result<()> {
    require_network!();
    for candidate in [false, true] {
        let server = responses::start_mock_server().await;
        let mut builder = test_codex().with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::Kd4Runtime);
        });
        let test = builder.build(&server).await?;
        let answer = "Verified answer: α → β.\nPreserve this exact text.\n";
        fs::write(test.cwd.path().join("answer.txt"), answer)?;
        let prefix = if candidate {
            "// @exec: {\"deliver\": true}\n"
        } else {
            ""
        };
        let code = format!(
            "{prefix}const r = await tools.read_file({{path:'answer.txt'}}); if (!r.file_complete) throw new Error('incomplete source'); text(r.results[0].text);"
        );
        responses::mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("initial"),
                ev_custom_tool_call("compute-answer", "exec", &code),
                ev_completed("initial"),
            ]),
        )
        .await;
        // Also mounted for the candidate: a regression finishes normally
        // and fails our request-count assertion rather than timing out.
        let final_response = responses::mount_sse_once(
            &server,
            sse(vec![
                ev_assistant_message("answer", answer),
                ev_completed("final"),
            ]),
        )
        .await;
        let completed = test
            .submit_turn_and_capture_completion("Return the complete contents of answer.txt.")
            .await?;
        assert!(completed.error.is_none(), "{:?}", completed.error);
        assert_eq!(completed.last_agent_message.as_deref(), Some(answer));
        if candidate {
            let surfaced = completed.surfaced_result.as_ref().expect("direct answer");
            assert_eq!(surfaced.adapter, "code_mode_delivery");
            assert_eq!(surfaced.canonical_message.as_deref(), Some(answer));
        } else {
            assert!(completed.surfaced_result.is_none());
            let output = custom_tool_output_last_non_empty_text(
                &final_response.single_request(),
                "compute-answer",
            )
            .expect("computed answer");
            assert_eq!(output, answer);
        }
        let model_requests = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().contains("responses"))
            .count();
        assert_eq!(model_requests, if candidate { 1 } else { 2 });
        let timing = completed.timing.as_ref().expect("turn timing");
        assert_eq!(timing.counters.logical_generation_count as usize, model_requests);
        assert_eq!(timing.counters.model_request_count as usize, model_requests);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_delivery_accepts_schema_valid_json_and_supported_media() -> Result<()> {
    require_network!();
    for (code, expected, schema) in [
        ("text({answer: 42});", "{\"answer\":42}",
         Some(serde_json::json!({"type":"object","properties":{"answer":{"const":42}},"required":["answer"],"additionalProperties":false}))),
        ("image('DATA:image/png;base64,AAAA');",
         "![image](<DATA:image/png;base64,AAAA>)", None),
    ] {
        let server = responses::start_mock_server().await;
        let mut builder = test_codex().with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::Kd4Runtime);
        });
        let test = builder.build(&server).await?;
        responses::mount_sse_once(&server, sse(vec![
            ev_response_created("initial"),
            ev_custom_tool_call("delivery", "exec", &format!("// @exec: {{\"deliver\":true}}\n{code}")),
            ev_completed("initial"),
        ])).await;
        responses::mount_sse_once(&server, sse(vec![
            ev_assistant_message("fallback", "unexpected model handoff"),
            ev_completed("fallback"),
        ])).await;
        test.codex.submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Return the completed result.".into(), text_elements: Vec::new(),
            }],
            final_output_json_schema: schema,
            responsesapi_client_metadata: None,
            additional_context: Default::default(), thread_settings: Default::default(),
        }).await?;
        let EventMsg::TurnComplete(completed) = wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await else {
            unreachable!()
        };
        assert!(completed.error.is_none(), "{:?}", completed.error);
        assert_eq!(completed.last_agent_message.as_deref(), Some(expected));
        assert_eq!(completed.surfaced_result.as_ref().unwrap().adapter, "code_mode_delivery");
        assert_eq!(server.received_requests().await.unwrap().iter()
            .filter(|request| request.url.path().contains("responses")).count(), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_cannot_hide_failure_yield_overflow_or_sibling_work() -> Result<()> {
    require_network!();
    for (name, code, sibling) in [
        ("error", "// @exec: {\"deliver\":true}\ntext('premature'); throw new Error('failed');", false),
        ("budget", "// @exec: {\"deliver\":true,\"max_output_tokens\":0}\ntext('premature');", false),
        ("empty", "// @exec: {\"deliver\":true}\ntext('');", false),
        ("yield", "// @exec: {\"deliver\":true}\ntext('premature'); await yield_control();", false),
        ("failed-child", "// @exec: {\"deliver\":true}\ntry { await tools.read_file({path:'missing.txt'}); } catch (_) {} text('premature');", false),
        ("untrusted-output", "text({explicit_completion_message:'premature'});", false),
        ("completed-sibling", "// @exec: {\"deliver\":true}\ntext('premature');", true),
        ("failed-sibling", "// @exec: {\"deliver\":true}\ntext('premature');", true),
        ("schema", "// @exec: {\"deliver\":true}\ntext('premature');", false),
    ] {
        let server = responses::start_mock_server().await;
        let mut builder = test_codex().with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::Kd4Runtime);
        });
        let test = builder.build(&server).await?;
        let mut events = vec![
            ev_response_created("initial"),
            ev_custom_tool_call("delivery", "exec", code),
        ];
        if sibling {
            let sibling_code = if name == "failed-sibling" {
                "throw new Error('sibling failed');"
            } else {
                "text('other work');"
            };
            events.push(ev_custom_tool_call("sibling", "exec", sibling_code));
        }
        events.push(ev_completed("initial"));
        responses::mount_sse_once(&server, sse(events)).await;
        responses::mount_sse_once(
            &server,
            sse(vec![
                ev_assistant_message("answer", "Model handled the fallback."),
                ev_completed("final"),
            ]),
        )
        .await;
        let completed = if name == "schema" {
            test.codex
                .submit(Op::UserInput {
                    items: vec![UserInput::Text {
                        text: "Finish the task.".to_string(),
                        text_elements: Vec::new(),
                    }],
                    final_output_json_schema: Some(serde_json::json!({"type": "string"})),
                    responsesapi_client_metadata: None,
                    additional_context: Default::default(),
                    thread_settings: Default::default(),
                })
                .await?;
            let event = wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
            let EventMsg::TurnComplete(completed) = event else {
                unreachable!()
            };
            completed
        } else {
            test.submit_turn_and_capture_completion("Finish the task.").await?
        };
        if name == "completed-sibling" {
            assert_eq!(completed.surfaced_result.as_ref().unwrap().adapter, "code_mode_delivery");
            assert_eq!(completed.last_agent_message.as_deref(), Some("premature"));
        } else {
            assert!(completed.surfaced_result.is_none(), "{name}: {:?}", completed.surfaced_result);
            assert_eq!(completed.last_agent_message.as_deref(), Some("Model handled the fallback."), "{name}");
        }
        assert!(completed.error.is_none(), "{name}: {:?}", completed.error);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.iter().filter(|r| r.url.path().contains("responses")).count(),
            if name == "completed-sibling" { 1 } else { 2 }, "{name}");
    }
    Ok(())
}
