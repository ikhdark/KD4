use super::*;
use crate::test_harness::responses;
use crate::test_harness::test_codex::test_codex;
use anyhow::Result;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use std::time::Duration;

fn run(f: impl Future<Output = Result<()>> + Send + 'static) -> Result<()> {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()?
                .block_on(f)
        })?
        .join()
        .expect("output regression thread panicked")
}

#[test]
fn ordinary_completion_repairs_missing_answer_and_bounds_repair() -> Result<()> {
    run(async {
        crate::test_harness::require_network!();
        for kind in ["empty", "reasoning", "commentary", "repeated-empty", "tool-repair"] {
            let server = responses::start_mock_server().await;
            let mut missing = vec![responses::ev_response_created("missing")];
            if kind == "commentary" {
                let mut message = responses::ev_assistant_message("status", "Still working.");
                message["item"]["phase"] = serde_json::json!("commentary");
                missing.push(message);
            } else if kind == "reasoning" {
                missing.push(responses::ev_reasoning_item(
                    "reasoning",
                    &["The result needs explanation."],
                    &[],
                ));
            }
            missing.push(responses::ev_completed("missing"));
            let repair = if kind == "tool-repair" {
                responses::sse(vec![
                    responses::ev_apply_patch_custom_tool_call(
                        "must-not-execute",
                        "*** Begin Patch\n*** Add File: duplicate-effect.txt\n+unexpected\n*** End Patch",
                    ),
                    responses::ev_completed("repair"),
                ])
            } else if kind == "repeated-empty" {
                responses::sse(vec![
                    responses::ev_response_created("repair"),
                    responses::ev_completed("repair"),
                ])
            } else {
                responses::sse(vec![
                    responses::ev_assistant_message(
                        "answer",
                        "The requested file contains the result.",
                    ),
                    responses::ev_completed("repair"),
                ])
            };
            let requests = responses::mount_sse_sequence(
                &server,
                vec![
                    responses::sse(vec![
                        responses::ev_apply_patch_custom_tool_call(
                            "write",
                            "*** Begin Patch\n*** Add File: result.txt\n+result\n*** End Patch",
                        ),
                        responses::ev_completed("tool"),
                    ]),
                    responses::sse(missing),
                    repair,
                ],
            )
            .await;
            let test = test_codex()
                .with_config(|config| {
                    config.features.disable(Feature::CodeModeHost).unwrap();
                })
                .build(&server)
                .await?;
            let completion = test
                .submit_turn_and_capture_completion("Write result.txt and explain the result.")
                .await?;
            assert_eq!(
                std::fs::read_to_string(test.workspace_path("result.txt"))?,
                "result\n"
            );
            assert_eq!(
                requests.requests().len(),
                3,
                "{kind}: one repair, not silent success or an unbounded loop"
            );
            assert!(requests.requests()[2].body_contains_text(MISSING_ANSWER_REPAIR_DIRECTIVE));
            assert_eq!(requests.requests()[2].body_json()["tool_choice"], "none");
            assert!(!test.workspace_path("duplicate-effect.txt").exists());
            if matches!(kind, "repeated-empty" | "tool-repair") {
                assert!(
                    completion.error.is_some(),
                    "missing answer must surface failure"
                );
                assert!(completion.last_agent_message.is_none());
            } else {
                assert!(completion.error.is_none(), "{kind}: {completion:?}");
                assert_eq!(
                    completion.last_agent_message.as_deref(),
                    Some("The requested file contains the result.")
                );
            }
        }
        Ok(())
    })
}

#[test]
fn prior_final_survives_an_unchanged_empty_continuation() -> Result<()> {
    run(async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let mut completed = responses::ev_completed("first");
        completed["response"]["end_turn"] = serde_json::json!(false);
        let requests = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message("retained", "The answer is four."),
                    completed,
                ]),
                responses::sse(vec![
                    responses::ev_response_created("empty"),
                    responses::ev_completed("empty"),
                ]),
            ],
        )
        .await;
        let test = test_codex().build(&server).await?;
        let completion = test
            .submit_turn_and_capture_completion("What is two plus two?")
            .await?;
        assert!(completion.error.is_none(), "{completion:?}");
        assert_eq!(
            completion.last_agent_message.as_deref(),
            Some("The answer is four.")
        );
        let history = test.codex.as_ref().codex.session.clone_history().await;
        assert_eq!(
            history
                .raw_items()
                .iter()
                .filter(|item| matches!(item,
                    ResponseItem::Message { id: Some(id), role, .. }
                        if id.as_str() == "retained" && role == "assistant"
                ))
                .count(),
            1,
            "the original final item is retained without republication"
        );
        assert_eq!(
            requests.requests().len(),
            2,
            "retention does not publish or sample again"
        );
        Ok(())
    })
}

struct PauseFinal {
    reached: CancellationToken,
    release: CancellationToken,
}

impl codex_extension_api::TurnItemContributor for PauseFinal {
    fn contribute<'a>(
        &'a self,
        _thread: &'a codex_extension_api::ExtensionData,
        _turn: &'a codex_extension_api::ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(message) = item
                && message.id == "before-mail"
            {
                self.reached.cancel();
                self.release.cancelled().await;
            }
            Ok(())
        })
    }
}

#[test]
fn continued_turn_reopens_mailbox_after_final_item() -> Result<()> {
    run(async {
        crate::test_harness::require_network!();
        for (phase, stop_hook) in [
            (None, false),
            (Some("final_answer"), false),
            (Some("commentary"), false),
            (None, true),
            (Some("final_answer"), true),
        ] {
            let server = responses::start_mock_server().await;
            let mut message = responses::ev_assistant_message("before-mail", "Initial result.");
            message["item"]["phase"] = serde_json::json!(phase);
            let mut completed = responses::ev_completed("continue");
            if !stop_hook {
                completed["response"]["end_turn"] = serde_json::json!(false);
            }
            let requests = responses::mount_sse_sequence(
                &server,
                vec![
                    responses::sse(vec![message, completed]),
                    responses::sse(vec![
                        responses::ev_assistant_message("corrected", "Correction incorporated."),
                        responses::ev_completed("corrected"),
                    ]),
                ],
            )
            .await;
            let reached = CancellationToken::new();
            let release = CancellationToken::new();
            let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
            extensions.turn_item_contributor(Arc::new(PauseFinal {
                reached: reached.clone(),
                release: release.clone(),
            }));
            let test = test_codex().with_extensions(Arc::new(extensions.build()))
                .with_pre_build_hook(move |home| {
                    if !stop_hook { return; }
                    let script = home.join("repair-once.py");
                    std::fs::write(&script, "import json,sys,pathlib\njson.load(sys.stdin)\np=pathlib.Path(__file__).with_suffix('.seen')\nfirst=not p.exists()\np.touch()\nprint(json.dumps({'decision':'block','reason':'incorporate the required correction'} if first else {}))\n").unwrap();
                    std::fs::write(home.join("hooks.json"), serde_json::json!({
                        "hooks": {"Stop": [{"hooks": [{"type":"command",
                            "command":format!("python3 \"{}\"", script.display()),
                            "commandWindows":format!("python \"{}\"", script.display())
                        }]}]}
                    }).to_string()).unwrap();
                })
                .with_config(move |config| {
                    if stop_hook { crate::test_harness::hooks::trust_discovered_hooks(config); }
                }).build(&server).await?;
            let sender = async {
                tokio::time::timeout(Duration::from_secs(10), reached.cancelled()).await?;
                let admission = test
                    .codex
                    .submit(Op::InterAgentCommunication {
                        communication: InterAgentCommunication::new(
                            codex_protocol::AgentPath::try_from("/root/worker").unwrap(),
                            codex_protocol::AgentPath::root(),
                            Vec::new(),
                            "required-mailbox-correction-137".to_string(),
                            true,
                        ),
                    })
                    .await;
                release.cancel();
                admission?;
                Ok::<(), anyhow::Error>(())
            };
            let (completion, sent) = tokio::join!(
                test.submit_turn_and_capture_completion("Finish the task."),
                sender
            );
            sent?;
            let completion = completion?;
            assert!(completion.error.is_none(), "{phase:?}: {completion:?}");
            let requests = requests.requests();
            assert_eq!(requests.len(), 2, "mail must not require a polling request");
            assert!(!requests[0].body_contains_text("required-mailbox-correction-137"));
            assert!(
                requests[1].body_contains_text("required-mailbox-correction-137"),
                "{phase:?}: next admitted prompt must include queued mail"
            );
        }
        Ok(())
    })
}

#[test]
fn explicit_no_prose_disposition_requires_a_completed_repair_item() -> Result<()> {
    run(async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let mut no_prose = responses::ev_assistant_message("intentional-silence", "");
        no_prose["item"]["phase"] = serde_json::json!("final_answer");
        let requests = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_response_created("empty"),
                    responses::ev_completed("empty"),
                ]),
                responses::sse(vec![no_prose, responses::ev_completed("declared")]),
            ],
        )
        .await;
        let test = test_codex().build(&server).await?;
        let completion = test
            .submit_turn_and_capture_completion("Do nothing and do not produce prose.")
            .await?;
        assert!(completion.error.is_none(), "{completion:?}");
        assert!(completion.last_agent_message.is_none());
        let history = test.codex.as_ref().codex.session.clone_history().await;
        assert!(
            history.raw_items().iter().any(|item| matches!(item,
                ResponseItem::Message { id: Some(id), role, content, .. }
                    if id.as_str() == "intentional-silence" && role == "assistant"
                        && content.iter().all(|part| matches!(part,
                            ContentItem::OutputText { text } if text.is_empty()))
            )),
            "intentional silence requires its completed repair item"
        );
        assert_eq!(requests.requests().len(), 2);
        Ok(())
    })
}

#[test]
fn true_terminal_keeps_nontriggering_mail_for_the_next_turn() -> Result<()> {
    run(async {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let requests = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message("before-mail", "First task complete."),
                    responses::ev_completed("first"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("next", "Second task complete."),
                    responses::ev_completed("next"),
                ]),
            ],
        )
        .await;
        let reached = CancellationToken::new();
        let release = CancellationToken::new();
        let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
        extensions.turn_item_contributor(Arc::new(PauseFinal {
            reached: reached.clone(),
            release: release.clone(),
        }));
        let test = test_codex()
            .with_extensions(Arc::new(extensions.build()))
            .build(&server)
            .await?;
        let sender = async {
            tokio::time::timeout(Duration::from_secs(10), reached.cancelled()).await?;
            let admission = test
                .codex
                .submit(Op::InterAgentCommunication {
                    communication: InterAgentCommunication::new(
                        codex_protocol::AgentPath::try_from("/root/worker").unwrap(),
                        codex_protocol::AgentPath::root(),
                        Vec::new(),
                        "next-turn-only-mail-137".to_string(),
                        false,
                    ),
                })
                .await;
            release.cancel();
            admission?;
            Ok::<(), anyhow::Error>(())
        };
        let (completion, sent) = tokio::join!(
            test.submit_turn_and_capture_completion("First task."),
            sender
        );
        sent?;
        assert!(completion?.error.is_none());
        assert_eq!(
            requests.requests().len(),
            1,
            "mail alone cannot manufacture an admission"
        );
        let completion = test
            .submit_turn_and_capture_completion("Second task.")
            .await?;
        assert!(completion.error.is_none());
        assert_eq!(requests.requests().len(), 2);
        assert!(requests.requests()[1].body_contains_text("next-turn-only-mail-137"));
        Ok(())
    })
}

#[tokio::test]
async fn cancelled_turn_does_not_admit_a_missing_answer_repair() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let turn = Arc::new(turn);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut budget = LogicalGenerationBudget::default();
    let result = run_turn(
        Arc::new(session),
        Arc::clone(&turn),
        Arc::clone(&turn.extension_data),
        Vec::new(),
        None,
        &mut budget,
        cancellation,
    )
    .await;
    assert!(matches!(result, Err(CodexErr::TurnAborted)));
    assert_eq!(budget.regular_generations, 0);
    assert!(!budget.terminal_generation_used);
}

#[tokio::test]
async fn output_observation_is_revision_bound_and_never_suppresses_publication() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let mut state = TurnOutputState::default();
    for (id, phase, revision, expected) in [
        (
            "one",
            Some("commentary"),
            "input-1",
            VisibleOutputNovelty::NewCommunication,
        ),
        (
            "two",
            Some("commentary"),
            "input-1",
            VisibleOutputNovelty::ExactRepeat,
        ),
        (
            "two",
            Some("commentary"),
            "input-1",
            VisibleOutputNovelty::Replay,
        ),
        (
            "three",
            Some("commentary"),
            "input-2",
            VisibleOutputNovelty::NewCommunication,
        ),
        (
            "final",
            Some("final_answer"),
            "input-2",
            VisibleOutputNovelty::NewCommunication,
        ),
        (
            "final-again",
            Some("final_answer"),
            "input-2",
            VisibleOutputNovelty::ExactRepeat,
        ),
    ] {
        let item = serde_json::from_value(serde_json::json!({
            "type":"message", "role":"assistant", "id":id, "phase":phase,
            "content":[{"type":"output_text", "text":"same visible text"}]
        }))
        .unwrap();
        let facts = finalize_non_tool_response_item(
            &session,
            TurnItemContributorPolicy::Skip,
            &item,
            false,
        )
        .await
        .unwrap()
        .facts;
        let candidate = facts
            .assistant_output
            .clone()
            .and_then(final_message_candidate_from_output);
        assert_eq!(
            state.observe(
                facts.assistant_output,
                candidate,
                revision.to_string(),
                true,
                true
            ),
            expected
        );
        assert_eq!(state.has_answer(), phase == Some("final_answer"));
    }
    assert_eq!(state.answer_text().as_deref(), Some("same visible text"));
    assert_eq!(
        state.observe(None, None, "input-2".to_string(), true, true),
        VisibleOutputNovelty::Unobserved
    );
    assert!(
        state.has_answer(),
        "silent inference does not erase delivered output"
    );
    state.reconcile_revision("new-source-or-input");
    assert!(
        !state.has_answer(),
        "changed evidence/input cannot reuse a stale answer"
    );
}
