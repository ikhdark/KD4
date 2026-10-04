//! Complete core turns; the provider is scripted, not live inference.
use super::assert_eq;
use super::*;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;

#[test_case::test_case("delivery")]
#[test_case::test_case("lookup")]
#[test_case::test_case("batch")]
#[test_case::test_case("recovery")]
#[test_case::test_case("poll")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_delivery_preserves_answer_and_removes_final_model_request(scenario: &str) -> Result<()> {
    require_network!();
    for candidate in [false, true] {
        eprintln!("direct-delivery scenario={scenario} candidate={candidate}");
        let server = responses::start_mock_server().await;
        let mut builder = test_codex().with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            let _ = config.features.enable(Feature::Kd4Runtime);
            config.background_terminal_max_timeout = 5_000;
        });
        let test = builder.build(&server).await?;
        let answer = "Verified answer: α → β.\nPreserve this exact text.\n";
        fs::write(test.cwd.path().join("answer.txt"), answer)?;
        fs::write(test.cwd.path().join("first.txt"), "Verified answer: α → β.\n")?;
        fs::write(test.cwd.path().join("second.txt"), "Preserve this exact text.\n")?;
        let bulk = serde_json::json!({"answer":answer,"padding":"filler ".repeat(900)}).to_string();
        fs::write(test.cwd.path().join("bulk.json"), &bulk)?;
        let read_answer = "const r = await tools.read_file({path:'answer.txt'}); if (!r.file_complete) throw new Error('incomplete source'); text(r.results[0].text);";
        let (prepare, compute) = match scenario {
            "delivery" => (None, read_answer.to_string()),
            "lookup" => (
                Some("text(resolve_tool('read_file').description);".to_string()),
                "const read = resolve_tool('read_file'); if (!read.description.includes('UTF-8')) throw Error('missing contract'); const r = await read({path:'answer.txt'}); if (!r.file_complete) throw Error('incomplete source'); text(r.results[0].text);".to_string(),
            ),
            "batch" => (
                Some("const r = await tools.read_file({path:'first.txt'}); if (!r.file_complete) throw Error('incomplete first file'); store('first', r.results[0].text); text('first file read');".to_string()),
                if candidate {
                    "const results = await Promise.allSettled(['first.txt','second.txt'].map(path => tools.read_file({path}))); for (const r of results) if (r.status !== 'fulfilled' || !r.value.file_complete) throw Error('incomplete batch'); text(results.map(r => r.value.results[0].text).join(''));".to_string()
                } else {
                    "const r = await tools.read_file({path:'second.txt'}); if (!r.file_complete) throw Error('incomplete second file'); text(load('first') + r.results[0].text);".to_string()
                },
            ),
            "recovery" => (
                Some("const head = await tools.read_file({path:'bulk.json', selectors:[{kind:'bytes',start:0,end:1}]}); store('artifact', head.artifact_id);".to_string()),
                format!("const r = await tools.read_tool_output({{artifact_id:load('artifact'),selectors:[{{kind:'bytes',start:0,end:{}}}]}}); if (!r.complete) throw Error('recovery required another handoff'); text(JSON.parse(r.results[0].text).answer);", bulk.len()),
            ),
            "poll" => {
                let python = which::which("python").or_else(|_| which::which("python3"))?;
                let command = serde_json::json!({
                    "program":python,
                    "args":["-X", "utf8", "-c", "import sys,time; time.sleep(8); sys.stdout.write(sys.argv[1])", answer],
                    "yield_time_ms":250,
                });
                (
                    Some(format!("const started = await tools.exec_command({command}); if (!started.session_id) throw Error('fixture must start a background process'); store('process', started.session_id);")),
                    // Output can precede process exit. Drain that mechanical
                    // transition in-cell, but reject every empty live result:
                    // those are the timer-only handoffs this regression targets.
                    "let output = ''; let r; do { r = await tools.write_stdin({session_id:load('process')}); if (!r.process_exited && !r.output) throw Error('timer-only handoff'); output += r.output; } while (!r.process_exited && r.session_id); if (!r.process_exited || r.exit_code !== 0) throw Error('command did not succeed'); text(output);".to_string(),
                )
            }
            _ => unreachable!(),
        };
        if !candidate && let Some(prepare) = &prepare {
            let extra = if scenario == "poll" {
                "const bounded = await tools.write_stdin({session_id:load('process'),wait_for_output:false}); if (bounded.process_exited) throw Error('fixture must cross a bounded poll'); text('still waiting');"
            } else {
                "text('prepared');"
            };
            responses::mount_sse_once(&server, sse(vec![
                ev_response_created("prepare"),
                ev_custom_tool_call("prepare", "exec", &format!("{prepare}\n{extra}")),
                ev_completed("prepare"),
            ])).await;
        }
        let prefix = if candidate {
            "// @exec: {\"deliver\": true,\"max_output_tokens\":64}\n"
        } else {
            ""
        };
        // Discovery and independent reads already have same-cell APIs. Recovery
        // and polling also need their owners not to manufacture a model boundary.
        let inline_prepare = if candidate && matches!(scenario, "recovery" | "poll") {
            prepare.as_deref().unwrap_or_default()
        } else {
            ""
        };
        let code = format!("{prefix}{inline_prepare}\n{compute}");
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
        let (sandbox_policy, permission_profile) =
            turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
        test.codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: format!("Complete the {scenario} task and return the exact verified answer."),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(AskForApproval::Never),
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    ..Default::default()
                },
            })
            .await?;
        let mut started = Vec::new();
        let mut messages = Vec::new();
        let completed = core_test_support::wait_for_event_with_timeout(
            &test.codex,
            |event| {
                match event {
                    EventMsg::ItemStarted(event) => {
                        if let TurnItem::AgentMessage(message) = &event.item {
                            started.push(message.id.clone());
                        }
                    }
                    EventMsg::ItemCompleted(event) => {
                        if let TurnItem::AgentMessage(message) = &event.item {
                            messages.push(message.clone());
                        }
                    }
                    _ => {}
                }
                matches!(event, EventMsg::TurnComplete(_))
            },
            Duration::from_secs(30),
        )
        .await;
        let EventMsg::TurnComplete(completed) = completed else {
            unreachable!()
        };
        assert!(completed.error.is_none(), "{:?}", completed.error);
        assert_eq!(completed.last_agent_message.as_deref(), Some(answer));
        assert_eq!(messages.len(), 1, "one visible answer before turn completion");
        assert_eq!(started, vec![messages[0].id.clone()]);
        assert!(matches!(
            messages[0].content.as_slice(),
            [AgentMessageContent::Text { text }] if text == answer
        ));
        if candidate {
            if messages[0].phase != Some(MessagePhase::FinalAnswer) {
                let request = final_response.single_request();
                let (output, success) = custom_tool_output_body_and_success(&request, "compute-answer");
                panic!("{scenario}: direct delivery fell back ({success:?}): {output}");
            }
        }
        test.codex.flush_rollout().await?;
        let (items, _, errors) = codex_core::RolloutRecorder::load_rollout_items(
            &test.codex.rollout_path().expect("rollout path"),
        )
        .await?;
        assert_eq!(errors, 0);
        let persisted = items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(ResponseItem::Message {
                    id, role, content, ..
                }) if role == "assistant" => Some((id, content)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(persisted.len(), 1, "the visible answer must survive reopening");
        assert_eq!(
            persisted[0].0.as_ref().map(|id| id.as_str()),
            Some(messages[0].id.as_str())
        );
        assert_eq!(
            persisted[0].1,
            &vec![ContentItem::OutputText {
                text: answer.to_string()
            }]
        );
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
        assert_eq!(model_requests, if candidate { 1 } else { 2 + usize::from(prepare.is_some()) }, "{scenario}");
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
        let EventMsg::TurnComplete(completed) = core_test_support::wait_for_event_with_timeout(
            &test.codex,
            |event| matches!(event, EventMsg::TurnComplete(_)),
            Duration::from_secs(30),
        ).await else {
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
        eprintln!("direct-delivery guard={name}");
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
