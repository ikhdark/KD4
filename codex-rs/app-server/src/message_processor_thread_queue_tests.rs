use super::*;
use codex_app_server_protocol::ThreadQueueAddResponse;
use codex_app_server_protocol::ThreadQueueDeleteResponse;
use codex_app_server_protocol::ThreadQueueListResponse;
use codex_app_server_protocol::ThreadQueueReorderResponse;
use codex_app_server_protocol::ThreadQueueStartResponse;
use codex_app_server_protocol::ThreadQueueUpdateResponse;
use pretty_assertions::assert_eq;

fn queue_request(id: i64, method: &str, params: serde_json::Value) -> ClientRequest {
    serde_json::from_value(json!({
        "id": id,
        "method": format!("thread/queue/{method}"),
        "params": params,
    }))
    .expect("registered queue request")
}

fn text_input(text: &str) -> serde_json::Value {
    json!([{"type":"text", "text":text, "text_elements":[]}])
}

#[test]
#[serial(app_server_tracing)]
fn thread_queue_rpc_lifecycle_and_automatic_follow_up() -> Result<()> {
    run_current_thread_test_with_stack(
        "thread_queue_rpc_lifecycle_and_automatic_follow_up",
        async {
            let server = create_mock_responses_server_repeating_assistant("Done").await;
            let mut harness = TracingHarness::new_with_features(
                server,
                Arc::new(codex_config::NoopThreadConfigLoader),
                false,
                true,
            )
            .await?;
            let started: ThreadStartResponse = harness
                .request(
                    ClientRequest::ThreadStart {
                        request_id: RequestId::Integer(10),
                        params: ThreadStartParams::default(),
                    },
                    None,
                )
                .await;
            read_thread_started_notification(&mut harness.outgoing_rx).await;
            let id = &started.thread.id;
            let thread_id = ThreadId::from_string(id)?;
            let thread = harness
                .processor
                .thread_manager
                .get_thread(thread_id)
                .await?;
            thread.ensure_rollout_materialized().await;
            thread.flush_rollout().await?;
            let db = thread.state_db().expect("persistent thread state");
            db.write_thread_queue(
                thread_id,
                r#"{"version":1,"revision":0,"submissions":[],"paused":true,"pendingStart":null}"#,
            )
            .await?;

            let empty: ThreadQueueListResponse = harness
                .request(
                    queue_request(11, "list", json!({"threadId":id,"cursor":null})),
                    None,
                )
                .await;
            assert!(empty.data.is_empty());
            let first: ThreadQueueAddResponse = harness
                .request(
                    queue_request(
                        12,
                        "add",
                        json!({
                            "threadId":id, "clientUserMessageId":"queue-first",
                            "input":text_input("Explain queued-first-original")
                        }),
                    ),
                    None,
                )
                .await;
            let second: ThreadQueueAddResponse = harness
                .request(
                    queue_request(
                        13,
                        "add",
                        json!({
                            "threadId":id, "clientUserMessageId":"queue-second",
                            "input":text_input("Explain queued-second")
                        }),
                    ),
                    None,
                )
                .await;
            let retry: ThreadQueueAddResponse = harness
                .request(
                    queue_request(
                        14,
                        "add",
                        json!({
                            "threadId":id, "clientUserMessageId":"queue-second",
                            "input":text_input("Explain queued-second")
                        }),
                    ),
                    None,
                )
                .await;
            assert_eq!(retry.queued_submission, second.queued_submission);
            let updated: ThreadQueueUpdateResponse = harness
                .request(
                    queue_request(
                        15,
                        "update",
                        json!({
                            "threadId":id, "queuedSubmissionId":first.queued_submission.id,
                            "input":text_input("Explain queued-first-edited")
                        }),
                    ),
                    None,
                )
                .await;
            assert_eq!(updated.queued_submission.id, first.queued_submission.id);
            let _: ThreadQueueReorderResponse = harness.request(queue_request(16, "reorder", json!({
                "threadId":id, "queuedSubmissionIds":[second.queued_submission.id,first.queued_submission.id]
            })), None).await;
            let page: ThreadQueueListResponse = harness
                .request(
                    queue_request(
                        17,
                        "list",
                        json!({
                            "threadId":id,"limit":1
                        }),
                    ),
                    None,
                )
                .await;
            assert_eq!(page.data, vec![second.queued_submission.clone()]);
            let last: ThreadQueueListResponse = harness
                .request(
                    queue_request(
                        18,
                        "list",
                        json!({
                            "threadId":id,"cursor":page.next_cursor
                        }),
                    ),
                    None,
                )
                .await;
            assert_eq!(last.data, vec![updated.queued_submission]);
            assert_eq!(last.next_cursor, None);
            let removed: ThreadQueueDeleteResponse = harness
                .request(
                    queue_request(
                        19,
                        "delete",
                        json!({
                            "threadId":id,"queuedSubmissionId":"unknown"
                        }),
                    ),
                    None,
                )
                .await;
            assert!(!removed.deleted);

            // A paused queue remains entirely durable and makes no model request.
            assert_eq!(harness._server.received_requests().await.unwrap().len(), 0);
            let response: ThreadQueueStartResponse = harness
                .request(
                    queue_request(
                        20,
                        "start",
                        json!({
                            "threadId":id,"queuedSubmissionId":second.queued_submission.id
                        }),
                    ),
                    None,
                )
                .await;
            assert!(!response.turn.id.is_empty());

            // The completion listener must drain the remaining entry without a
            // second queue/start request. Read notifications while it runs.
            let mut completed = HashSet::new();
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                while completed.len() < 2 {
                    let envelope = harness
                        .outgoing_rx
                        .recv()
                        .await
                        .expect("notification channel");
                    if let crate::outgoing_message::OutgoingEnvelope::ToConnection {
                        message:
                            crate::outgoing_message::OutgoingMessage::AppServerNotification(
                                codex_app_server_protocol::ServerNotification::TurnCompleted(
                                    notification,
                                ),
                            ),
                        ..
                    } = envelope
                    {
                        assert_eq!(notification.thread_id, *id);
                        assert_eq!(
                            notification.turn.status,
                            codex_app_server_protocol::TurnStatus::Completed,
                            "queued turn failed: {:?}",
                            notification.turn.error
                        );
                        assert!(
                            completed.insert(notification.turn.id),
                            "a turn completes only once"
                        );
                    }
                }
            })
            .await?;
            assert!(completed.contains(&response.turn.id));
            // Simulate a crash after core accepted a queued turn but before its
            // durable consume completed. Recovery must not replay that prompt.
            thread.flush_rollout().await?;
            db.write_thread_queue(
                thread_id,
                &json!({
                    "version":1, "revision":50, "paused":false,
                    "submissions":[second.queued_submission],
                    "pendingStart":{
                        "submissionId":second.queued_submission.id,
                        "turnId":response.turn.id
                    }
                })
                .to_string(),
            )
            .await?;
            let empty: ThreadQueueListResponse = harness
                .request(queue_request(21, "list", json!({"threadId":id})), None)
                .await;
            assert!(empty.data.is_empty());
            let requests = harness._server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2, "one model request per queued submission");
            let first_body = String::from_utf8_lossy(&requests[0].body);
            let second_body = String::from_utf8_lossy(&requests[1].body);
            assert!(first_body.contains("queued-second"));
            assert!(!first_body.contains("queued-first-original"));
            assert!(second_body.contains("queued-first-edited"));
            harness.shutdown().await;
            Ok(())
        },
    )
}

#[test]
#[serial(app_server_tracing)]
fn thread_queue_active_turn_rejection_and_interrupt_preserve_input() -> Result<()> {
    run_current_thread_test_with_stack(
        "thread_queue_active_turn_rejection_and_interrupt_preserve_input",
        async {
            use crate::outgoing_message::ConnectionRequestId;
            use crate::request_processors::QueueOrigin;
            use codex_app_server_protocol::ThreadQueueStartParams;
            use codex_app_server_protocol::TurnInterruptResponse;
            use std::time::Duration;

            let server = MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/v1/responses"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(
                            app_test_support::create_final_assistant_message_sse_response("Done")?,
                        )
                        .set_delay(Duration::from_secs(60)),
                )
                .mount(&server)
                .await;
            let mut harness = TracingHarness::new_with_features(
                server,
                Arc::new(codex_config::NoopThreadConfigLoader),
                false,
                true,
            )
            .await?;
            let started: ThreadStartResponse = harness
                .request(
                    ClientRequest::ThreadStart {
                        request_id: RequestId::Integer(100),
                        params: ThreadStartParams::default(),
                    },
                    None,
                )
                .await;
            read_thread_started_notification(&mut harness.outgoing_rx).await;
            let id = &started.thread.id;
            let turn: TurnStartResponse = harness
                .request(
                    ClientRequest::TurnStart {
                        request_id: RequestId::Integer(101),
                        params: serde_json::from_value(json!({
                            "threadId":id,"input":text_input("active-original")
                        }))?,
                    },
                    None,
                )
                .await;
            let queued: ThreadQueueAddResponse = harness
                .request(
                    queue_request(
                        102,
                        "add",
                        json!({
                            "threadId":id,"clientUserMessageId":"retained-after-interrupt",
                            "input":text_input("do-not-steer-this")
                        }),
                    ),
                    None,
                )
                .await;
            let error = harness
                .processor
                .thread_queue_processor
                .start(
                    ThreadQueueStartParams {
                        thread_id: id.clone(),
                        queued_submission_id: queued.queued_submission.id.clone(),
                    },
                    QueueOrigin {
                        request_id: ConnectionRequestId {
                            connection_id: TEST_CONNECTION_ID,
                            request_id: RequestId::Integer(103),
                        },
                        client_name: None,
                        client_version: None,
                        supports_openai_form_elicitation: false,
                    },
                )
                .await
                .expect_err("queue/start cannot steer an active turn");
            assert_eq!(
                error.data.as_ref().and_then(|data| data["reason"].as_str()),
                Some("activeTurnInProgress")
            );
            let _: TurnInterruptResponse = harness
                .request(
                    ClientRequest::TurnInterrupt {
                        request_id: RequestId::Integer(104),
                        params: serde_json::from_value(
                            json!({"threadId":id,"turnId":turn.turn.id}),
                        )?,
                    },
                    None,
                )
                .await;
            let page: ThreadQueueListResponse = harness
                .request(queue_request(105, "list", json!({"threadId":id})), None)
                .await;
            assert_eq!(page.data, vec![queued.queued_submission.clone()]);
            let thread_id = ThreadId::from_string(id)?;
            let thread = harness
                .processor
                .thread_manager
                .get_thread(thread_id)
                .await?;
            let payload = thread
                .state_db()
                .expect("state")
                .read_thread_queue(thread_id)
                .await?
                .expect("queue retained");
            let stored: serde_json::Value = serde_json::from_str(&payload)?;
            assert_eq!(stored["paused"], true);
            let removed: ThreadQueueDeleteResponse = harness
                .request(
                    queue_request(
                        106,
                        "delete",
                        json!({
                            "threadId":id,"queuedSubmissionId":queued.queued_submission.id
                        }),
                    ),
                    None,
                )
                .await;
            assert!(removed.deleted);
            let page: ThreadQueueListResponse = harness
                .request(queue_request(107, "list", json!({"threadId":id})), None)
                .await;
            assert!(page.data.is_empty());
            for request in harness._server.received_requests().await.unwrap() {
                assert!(!String::from_utf8_lossy(&request.body).contains("do-not-steer-this"));
            }
            harness.shutdown().await;
            Ok(())
        },
    )
}
