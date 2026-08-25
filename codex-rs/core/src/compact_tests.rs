use super::*;
use crate::session::tests::build_world_state_from_turn_context;
use codex_context_fragments::ContextualUserFragment;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::ResponseItemId;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;

async fn process_compacted_history_with_test_session(
    compacted_history: Vec<ResponseItem>,
    previous_turn_settings: Option<&PreviousTurnSettings>,
) -> (Vec<ResponseItem>, Vec<ResponseItem>) {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    session
        .set_previous_turn_settings(previous_turn_settings.cloned())
        .await;
    let world_state = Arc::new(build_world_state_from_turn_context(&session, &turn_context).await);
    let initial_context = session
        .build_initial_context_with_world_state(&turn_context, world_state.as_ref())
        .await;
    let initial_context_injection = InitialContextInjection::BeforeLastUserMessage(world_state);
    let (refreshed, _, _) = crate::compact_remote::process_compacted_history(
        &session,
        &turn_context,
        compacted_history,
        &initial_context_injection,
    )
    .await;
    (refreshed, initial_context)
}

#[tokio::test]
async fn compaction_initial_context_carries_only_delivered_world_state_snapshot() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut world_state = WorldState::default();
    for (index, (id, body)) in [("large_0", 'a'), ("large_1", 'b')].into_iter().enumerate() {
        world_state.add_extension_section(WorldStateSectionContribution::new(
            id,
            json!({"value": index}),
            move |previous| match previous {
                PreviousWorldStateSection::Absent => Some(RenderedWorldStateFragment::new(
                    "developer",
                    ("", ""),
                    body.to_string().repeat(30_000),
                )),
                _ => None,
            },
        ));
    }
    let world_state = Arc::new(world_state);
    let injection = InitialContextInjection::BeforeLastUserMessage(Arc::clone(&world_state));

    let (_, Some(delivered_snapshot), _) =
        build_compaction_initial_context(&session, &turn_context, &injection).await
    else {
        panic!("mid-turn compaction should carry a delivered world-state snapshot");
    };

    assert_eq!(
        delivered_snapshot.clone().into_value(),
        json!({"large_0": {"value": 0}})
    );
    let (retry, final_snapshot) = world_state.render_diff_with_snapshot(&delivered_snapshot);
    assert_eq!(retry.len(), 1);
    assert_eq!(final_snapshot, world_state.snapshot());
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn agent_message(text: &str) -> ResponseItem {
    ResponseItem::AgentMessage {
        id: None,
        author: "worker".to_string(),
        recipient: "root".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: text.to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    }
}

fn compacted_user_message(text: &str) -> CompactedUserMessage {
    CompactedUserMessage {
        source_item_id: None,
        content: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn content_items_to_text_joins_non_empty_segments() {
    let items = vec![
        ContentItem::InputText {
            text: "hello".to_string(),
        },
        ContentItem::OutputText {
            text: String::new(),
        },
        ContentItem::OutputText {
            text: "world".to_string(),
        },
    ];

    let joined = content_items_to_text(&items);

    assert_eq!(Some("hello\nworld".to_string()), joined);
}

#[test]
fn content_items_to_text_ignores_image_only_content() {
    let items = vec![ContentItem::InputImage {
        image_url: "file://image.png".to_string(),
        detail: Some(DEFAULT_IMAGE_DETAIL),
    }];

    let joined = content_items_to_text(&items);

    assert_eq!(None, joined);
}

#[test]
fn collect_user_messages_extracts_user_text_only() {
    let items = vec![
        ResponseItem::Message {
            id: Some(ResponseItemId::with_suffix("msg", "assistant")),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "ignored".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: Some(ResponseItemId::with_suffix("msg", "user")),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "first".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Other,
    ];

    let collected = collect_user_messages(&items);

    assert_eq!(vec![compacted_user_message("first")], collected);
}

#[test]
fn collect_unresolved_user_messages_keeps_only_tail_after_model_output() {
    let items = vec![
        user_message("consumed request"),
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "completed response".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        user_message("unresolved exact constraint"),
    ];

    let collected = collect_unresolved_user_messages(&items);

    assert_eq!(
        collected,
        vec![compacted_user_message("unresolved exact constraint")]
    );
}

#[test]
fn compacted_history_preserves_mixed_and_image_only_user_requirements() {
    let metadata = InternalChatMessageMetadataPassthrough {
        turn_id: Some("turn-with-image".to_string()),
    };
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: r#"<image name=[Image #1] path="C:\private\original.png">"#.to_string(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,mixed".to_string(),
                    detail: Some(codex_protocol::models::ImageDetail::Low),
                },
                ContentItem::InputText {
                    text: "</image>".to_string(),
                },
                ContentItem::InputText {
                    text: "compare this image".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(metadata.clone()),
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,image-only".to_string(),
                detail: Some(codex_protocol::models::ImageDetail::Original),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let collected = collect_user_messages(&items);
    let history = build_compacted_history(Vec::new(), &collected, "SUMMARY");

    assert_eq!(collected.len(), 2);
    assert_eq!(
        collected[0].content,
        vec![
            UserInput::Image {
                image_url: "data:image/png;base64,mixed".to_string(),
                detail: Some(codex_protocol::models::ImageDetail::Low),
            },
            UserInput::Text {
                text: "compare this image".to_string(),
                text_elements: Vec::new(),
            },
        ]
    );
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &history[0]
    else {
        panic!("expected rebuilt mixed user message");
    };
    assert_eq!(
        content,
        &vec![
            ContentItem::InputImage {
                image_url: "data:image/png;base64,mixed".to_string(),
                detail: Some(codex_protocol::models::ImageDetail::Low),
            },
            ContentItem::InputText {
                text: "compare this image".to_string(),
            },
        ]
    );
    assert_eq!(
        internal_chat_message_metadata_passthrough.as_ref(),
        Some(&metadata)
    );
    assert!(format!("{history:?}").contains("image-only"));
    assert!(!format!("{history:?}").contains("private\\original.png"));
}

#[test]
fn compaction_strips_tagged_startup_entries_but_retains_untagged_legacy_text() {
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: crate::context::TaskModelGuidance.render(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: r#"# AGENTS.md instructions for project

<INSTRUCTIONS>
do things
</INSTRUCTIONS>"#
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "legacy environment_context: cwd=/tmp".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "real user message".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let compactable = strip_compaction_startup_envelopes(items);
    let collected = collect_user_messages(&compactable);

    assert_eq!(
        vec![
            compacted_user_message("legacy environment_context: cwd=/tmp"),
            compacted_user_message("real user message"),
        ],
        collected
    );
}

#[test]
fn collect_user_messages_filters_legacy_warnings() {
    let items = vec![
        user_message(
            "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
        ),
        user_message(
            "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command.",
        ),
        user_message(
            "Warning: Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: https://chatgpt.com/cyber or learn more: https://developers.openai.com/codex/concepts/cyber-safety",
        ),
        user_message("real user message"),
    ];

    let collected = collect_user_messages(&items);

    assert_eq!(vec![compacted_user_message("real user message")], collected);
}

#[test]
fn build_token_limited_compacted_history_truncates_overlong_user_messages() {
    // Use a small truncation limit so the test remains fast while still validating
    // that oversized user content is truncated.
    let max_tokens = 16;
    let big = "word ".repeat(200);
    let user_message = compacted_user_message(&big);
    let history = super::build_compacted_history_with_limit(
        Vec::new(),
        std::slice::from_ref(&user_message),
        "SUMMARY",
        max_tokens,
    );
    assert_eq!(history.len(), 2);

    let truncated_message = &history[0];
    let summary_message = &history[1];

    let truncated_text = match truncated_message {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content_items_to_text(content).unwrap_or_default()
        }
        other => panic!("unexpected item in history: {other:?}"),
    };

    assert!(
        truncated_text.contains("tokens truncated"),
        "expected truncation marker in truncated user message"
    );
    assert!(
        !truncated_text.contains(&big),
        "truncated user message should not include the full oversized user text"
    );

    let summary_text = match summary_message {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content_items_to_text(content).unwrap_or_default()
        }
        other => panic!("unexpected item in history: {other:?}"),
    };
    assert_eq!(summary_text, "SUMMARY");
}

#[test]
fn local_compaction_enforces_user_intent_and_task_state_budgets() {
    let user_messages = vec![compacted_user_message(&"user intent ".repeat(8_000))];
    let history = build_compacted_history(
        Vec::new(),
        &user_messages,
        &bounded_task_state_summary(None, &"active requirement ".repeat(8_000)),
    );

    let user_text = match &history[0] {
        ResponseItem::Message { content, .. } => content_items_to_text(content).unwrap_or_default(),
        other => panic!("expected user intent message, got {other:?}"),
    };
    let task_state = match history.last() {
        Some(ResponseItem::Message { content, .. }) => {
            content_items_to_text(content).unwrap_or_default()
        }
        other => panic!("expected task-state message, got {other:?}"),
    };

    assert!(approx_token_count(&user_text) <= COMPACT_USER_MESSAGE_MAX_TOKENS);
    assert!(approx_token_count(&task_state) <= COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(task_state.starts_with(SUMMARY_PREFIX));
}

#[test]
fn incremental_compaction_preserves_the_previous_summary_prefix() {
    let previous = format!("{SUMMARY_PREFIX}\nverified state");
    let summary = bounded_task_state_summary(Some(&previous), "new unresolved item");

    assert!(summary.starts_with(&previous));
    assert_eq!(summary, format!("{previous}\n\nnew unresolved item"));
}

#[test]
fn semantic_summary_truncation_preserves_conversation_state() {
    let intent = "INTENT-SENTINEL";
    let unresolved = "UNRESOLVED-SENTINEL";
    let generated = format!(
        "{GOAL_HEADING}\n{intent}\n{}\n\n{CURRENT_STATE_HEADING}\nworking\n\n{COMPLETED_WORK_HEADING}\ndone\n\n{UNRESOLVED_WORK_HEADING}\n{unresolved}\n{}\n\n{EVIDENCE_HEADING}\nverified\n\n{NEXT_ACTION_HEADING}\ncontinue",
        "current detail ".repeat(2_000),
        "hypothesis detail ".repeat(2_000),
    );

    let summary = bounded_task_state_summary(None, &generated);

    assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(summary.contains(intent));
    assert!(summary.contains(unresolved));
}

#[test]
fn final_bounded_compaction_preserves_all_required_sections() {
    let generated = COMPACTION_SECTIONS
        .iter()
        .map(|(heading, _)| format!("{heading}\n{}", "section evidence ".repeat(2_000)))
        .collect::<Vec<_>>()
        .join("\n\n");

    let summary = bounded_task_state_summary(None, &generated);

    assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    for (heading, _) in COMPACTION_SECTIONS {
        assert!(
            section_has_nonempty_body(&summary, heading),
            "bounded checkpoint lost {heading}: {summary}"
        );
    }
    assert!(validate_generated_compaction_summary(None, &summary).is_ok());
}

#[test]
fn compaction_section_allocations_fit_without_final_blind_cut() {
    let generated = format!(
        "{}\n{}",
        "preamble ".repeat(1_000),
        COMPACTION_SECTIONS
            .iter()
            .map(|(heading, _)| format!("{heading}\n{}", "bounded content ".repeat(1_000)))
            .collect::<Vec<_>>()
            .join("\n\n")
    );

    let summary = truncate_compaction_summary(&generated, COMPACT_TASK_STATE_MAX_TOKENS);

    assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(summary.contains(NEXT_ACTION_HEADING));
    assert!(section_has_nonempty_body(&summary, NEXT_ACTION_HEADING));
}

#[test]
fn semantic_summary_truncation_prefers_newest_updates() {
    let previous = format!(
        "{SUMMARY_PREFIX}\n{CURRENT_STATE_HEADING}\n{}",
        "obsolete active state ".repeat(8_000)
    );
    let newest = format!(
        "{CURRENT_STATE_HEADING}\nLATEST-ACTIVE-SENTINEL\n{}",
        "current detail ".repeat(300)
    );

    let summary = bounded_task_state_summary(Some(&previous), &newest);

    assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(summary.contains("LATEST-ACTIVE-SENTINEL"));
}

#[test]
fn unstructured_incremental_summary_preserves_the_newest_update() {
    let previous = format!(
        "{SUMMARY_PREFIX}\n{CURRENT_STATE_HEADING}\n{}",
        "obsolete state ".repeat(8_000)
    );

    let summary = bounded_task_state_summary(
        Some(&previous),
        "LATEST-UNSTRUCTURED-SENTINEL unresolved constraint",
    );

    assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(summary.contains("LATEST-UNSTRUCTURED-SENTINEL"));
}

#[test]
fn inline_heading_mentions_do_not_trigger_structured_summary_budgeting() {
    let summary = format!(
        "The phrase `{CURRENT_STATE_HEADING}` is documentation, not a section. {} END-SENTINEL",
        "detail ".repeat(1_000)
    );

    let truncated = truncate_compaction_summary(&summary, COMPACT_TASK_STATE_MAX_TOKENS);

    assert!(truncated.contains("END-SENTINEL"));
    assert!(approx_token_count(&truncated) > 300);
}

#[test]
fn generated_compaction_accepts_legacy_handoffs_and_validates_structured_checkpoints() {
    let complete = format!(
        "{GOAL_HEADING}\nfinish recovery\n\n{CURRENT_STATE_HEADING}\nimplementation present\n\n{COMPLETED_WORK_HEADING}\nproducer updated\n\n{UNRESOLVED_WORK_HEADING}\nkeep ambiguity\n\n{EVIDENCE_HEADING}\nfocused evidence\n\n{NEXT_ACTION_HEADING}\nrun focused proof"
    );
    assert!(validate_generated_compaction_summary(None, &complete).is_ok());

    let incomplete = format!(
        "{GOAL_HEADING}\nfinish recovery\n\n{CURRENT_STATE_HEADING}\nimplementation present\n\n{COMPLETED_WORK_HEADING}\nproducer updated\n\n{UNRESOLVED_WORK_HEADING}\nkeep ambiguity\n\n{EVIDENCE_HEADING}\nfocused evidence\n\n{NEXT_ACTION_HEADING}\n"
    );
    assert!(validate_generated_compaction_summary(None, &incomplete).is_err());
    assert!(validate_generated_compaction_summary(None, "free-form checkpoint").is_ok());
    assert!(validate_generated_compaction_summary(Some(&complete), "free-form update").is_ok());
    assert!(
        validate_generated_compaction_summary(
            Some(&complete),
            &format!("{UNRESOLVED_WORK_HEADING}\nnew unresolved item")
        )
        .is_ok()
    );
}

#[test]
fn unresolved_agent_messages_survive_compaction_as_native_items() {
    let unresolved_agent = agent_message("worker evidence that root has not consumed");
    let items = vec![
        user_message("consumed request"),
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "completed response".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        user_message("unresolved request"),
        unresolved_agent.clone(),
    ];

    assert_eq!(
        collect_unresolved_agent_messages(&items),
        vec![unresolved_agent.clone()]
    );
    let (history, _) = build_unresolved_user_history(&items);
    assert_eq!(
        history,
        vec![user_message("unresolved request"), unresolved_agent]
    );
}

#[test]
fn unresolved_user_and_agent_messages_keep_their_original_order() {
    let unresolved_agent = agent_message("worker result awaiting root review");
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "previous model output".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        unresolved_agent.clone(),
        user_message("newer user constraint"),
    ];

    let (history, _) = build_unresolved_user_history(&items);

    assert_eq!(
        history,
        vec![unresolved_agent, user_message("newer user constraint")]
    );
}

#[test]
fn summary_reuse_is_disabled_when_post_summary_user_tail_is_truncated() {
    let items = vec![
        user_message(&format!("{SUMMARY_PREFIX}\nprior checkpoint")),
        user_message(&"unresolved constraint ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS * 3)),
    ];

    let (_, _, _, omitted_user_text) = build_bounded_unresolved_input_history(&items);

    assert!(omitted_user_text);
    assert!(!can_reuse_previous_summary(&items, omitted_user_text));
}

#[test]
fn bounded_user_history_emits_text_omission_receipt() {
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "previous response".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        user_message(&"exact constraint ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS * 3)),
    ];

    let (history, _, _, omitted_user_text) = build_bounded_unresolved_input_history(&items);
    let rendered = serde_json::to_string(&history).expect("history serializes");

    assert!(omitted_user_text);
    assert!(rendered.contains(COMPACT_TEXT_OMISSION_MARKER));
    assert!(rendered.contains("\"role\":\"user\""));
}

#[test]
fn unresolved_text_omission_reports_stable_provenance_and_exact_counts() {
    let text = "exact constraint ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS * 3);
    let original_tokens = approx_token_count(&text);
    let items = vec![ResponseItem::Message {
        id: Some(ResponseItemId::from_server("user-message-7".to_string())),
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("turn-7".to_string()),
        }),
    }];

    let (history, _, _, omitted_user_text) = build_bounded_unresolved_input_history(&items);
    let receipt = history
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => {
                content.iter().find_map(|content| match content {
                    ContentItem::InputText { text }
                        if text.contains(COMPACT_TEXT_OMISSION_MARKER) =>
                    {
                        serde_json::from_str::<serde_json::Value>(text).ok()
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .next()
        .expect("typed omission receipt");

    assert!(omitted_user_text);
    assert_eq!(receipt["source_item_id"], "user-message-7");
    assert_eq!(receipt["turn_id"], "turn-7");
    assert_eq!(receipt["original_tokens"], original_tokens);
    let retained = receipt["retained_tokens"]
        .as_u64()
        .expect("retained tokens") as usize;
    assert_eq!(
        receipt["omitted_tokens"],
        original_tokens.saturating_sub(retained)
    );
    assert_eq!(receipt["unresolved"], true);
}

#[test]
fn bounded_agent_history_emits_text_omission_receipt() {
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "previous response".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        agent_message(&"worker evidence ".repeat(COMPACT_AGENT_MESSAGE_MAX_TOKENS * 3)),
    ];

    let (history, _, _, _) = build_bounded_unresolved_input_history(&items);
    let rendered = serde_json::to_string(&history).expect("history serializes");

    assert!(rendered.contains(COMPACT_TEXT_OMISSION_MARKER));
    assert!(rendered.contains("\"role\":\"agent\""));
}

#[test]
fn unresolved_tool_output_survives_local_compaction_as_typed_receipt() {
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "inspect".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "pending-call".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "pending-call".to_string(),
        output: codex_protocol::models::FunctionCallOutputPayload::from_text(
            "exact pending evidence".to_string(),
        ),
        internal_chat_message_metadata_passthrough: None,
    };
    let items = vec![user_message("request"), call.clone(), output.clone()];

    let (history, _, _, _) = build_bounded_unresolved_input_history(&items);

    assert_eq!(history, vec![call, output]);
}

#[test]
fn newest_section_updates_include_separator_cost_in_their_budget() {
    let updates = (0..200)
        .map(|index| vec![format!("update-{index}")])
        .collect::<Vec<_>>();

    let retained = retain_newest_section_updates(&updates, 32);

    assert!(approx_token_count(&retained) <= 32);
    assert!(retained.contains("update-199"));
    assert!(!retained.lines().any(|line| line == "update-0"));
}

#[test]
fn summary_can_be_reused_when_only_new_user_input_follows_it() {
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsettled state"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "next request".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert!(history_after_latest_summary_is_user_only(&items));
}

#[test]
fn at_start_injection_preserves_cacheable_prefix_order() {
    let prefix = vec![user_message("stable prefix")];
    let compacted_history = vec![user_message("retained history"), user_message("summary")];
    let injection = InitialContextInjection::AtStart(Arc::new(WorldState::default()));

    let refreshed =
        insert_compaction_initial_context(compacted_history.clone(), prefix.clone(), &injection);

    assert_eq!(refreshed, [prefix, compacted_history].concat());
}

#[test]
fn text_truncation_keeps_images_in_their_original_order() {
    let message = CompactedUserMessage {
        source_item_id: None,
        content: vec![
            UserInput::Text {
                text: "word ".repeat(200),
                text_elements: Vec::new(),
            },
            UserInput::Image {
                image_url: "data:image/png;base64,retained".to_string(),
                detail: Some(codex_protocol::models::ImageDetail::High),
            },
            UserInput::Text {
                text: "older text outside the budget".to_string(),
                text_elements: Vec::new(),
            },
        ],
        internal_chat_message_metadata_passthrough: None,
    };

    let history = super::build_compacted_history_with_limit(Vec::new(), &[message], "SUMMARY", 8);
    let ResponseItem::Message { content, .. } = &history[0] else {
        panic!("expected rebuilt user message");
    };

    assert!(matches!(
        content.first(),
        Some(ContentItem::InputText { .. })
    ));
    assert!(matches!(
        content.get(1),
        Some(ContentItem::InputImage { image_url, .. }) if image_url.ends_with("retained")
    ));
}

#[test]
fn image_limits_emit_a_stable_compaction_omission_marker() {
    let images = (0..3)
        .map(|index| UserInput::Image {
            image_url: format!("img-{index}"),
            detail: None,
        })
        .collect();
    let message = CompactedUserMessage {
        source_item_id: None,
        content: images,
        internal_chat_message_metadata_passthrough: None,
    };

    let history =
        super::build_compacted_history_with_limits(Vec::new(), &[message], "SUMMARY", 0, 2, 9);
    let retained_images = match &history[0] {
        ResponseItem::Message { content, .. } => content
            .iter()
            .filter(|item| matches!(item, ContentItem::InputImage { .. }))
            .count(),
        other => panic!("expected rebuilt user message, found {other:?}"),
    };
    let summary = match history.last() {
        Some(ResponseItem::Message { content, .. }) => {
            content_items_to_text(content).unwrap_or_default()
        }
        other => panic!("expected summary message, found {other:?}"),
    };

    assert_eq!(retained_images, 1);
    assert!(summary.contains(COMPACT_IMAGE_OMISSION_MARKER));
}

#[test]
fn build_token_limited_compacted_history_appends_summary_message() {
    let initial_context: Vec<ResponseItem> = Vec::new();
    let user_messages = vec![compacted_user_message("first user message")];
    let summary_text = "summary text";

    let history = build_compacted_history(initial_context, &user_messages, summary_text);
    assert!(
        !history.is_empty(),
        "expected compacted history to include summary"
    );

    let last = history.last().expect("history should have a summary entry");
    let summary = match last {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content_items_to_text(content).unwrap_or_default()
        }
        other => panic!("expected summary message, found {other:?}"),
    };
    assert_eq!(summary, summary_text);
}

#[test]
fn build_compacted_history_preserves_user_message_passthrough_metadata() {
    let history = build_compacted_history(
        Vec::new(),
        &[CompactedUserMessage {
            source_item_id: None,
            content: vec![UserInput::Text {
                text: "first user message".to_string(),
                text_elements: Vec::new(),
            }],
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    turn_id: Some("turn-1".to_string()),
                },
            ),
        }],
        "summary text",
    );

    assert_eq!(history[0].turn_id(), Some("turn-1"));
    assert_eq!(history[1].turn_id(), None);
}

#[test]
fn local_compaction_attempt_buffers_output_until_completed() {
    let partial = user_message("partial summary");
    let mut accumulator = LocalCompactionAccumulator::default();

    accumulator.record_output(partial.clone());
    assert_eq!(accumulator.items, vec![partial.clone()]);

    let output = accumulator.complete(None);
    assert_eq!(output.items, vec![partial]);
    assert_eq!(output.token_usage, None);
}

#[test]
fn should_use_remote_compact_task_for_azure_provider() {
    let provider = ModelProviderInfo {
        name: "Azure".into(),
        base_url: Some("https://example.com/openai".into()),
        env_key: Some("AZURE_OPENAI_API_KEY".into()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    };

    assert!(should_use_remote_compact_task(&provider));
}
#[tokio::test]
async fn process_compacted_history_replaces_developer_messages() {
    let compacted_history = vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "stale permissions".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "stale personality".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let (refreshed, expected) = process_compacted_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    assert_eq!(refreshed, expected);
}

#[tokio::test]
async fn process_compacted_history_reinjects_full_initial_context() {
    let compacted_history = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let (refreshed, expected) = process_compacted_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    assert_eq!(refreshed, expected);
}

#[tokio::test]
async fn process_compacted_history_drops_non_user_content_messages() {
    let compacted_history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: r#"# AGENTS.md instructions for /repo

<INSTRUCTIONS>
keep me updated
</INSTRUCTIONS>"#
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: r#"<environment_context>
  <cwd>/repo</cwd>
  <shell>zsh</shell>
</environment_context>"#
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: r#"<turn_aborted>
  <turn_id>turn-1</turn_id>
  <reason>interrupted</reason>
</turn_aborted>"#
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "stale developer instructions".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let (refreshed, expected) = process_compacted_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    assert_eq!(refreshed, expected);
}

#[tokio::test]
async fn process_compacted_history_drops_legacy_warnings() {
    let compacted_history = vec![
        user_message(
            "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
        ),
        user_message(
            "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command.",
        ),
        user_message(
            "Warning: Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: https://chatgpt.com/cyber or learn more: https://developers.openai.com/codex/concepts/cyber-safety",
        ),
        user_message("latest user"),
    ];
    let (refreshed, initial_context) = process_compacted_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    assert_eq!(refreshed, initial_context);
}

#[tokio::test]
async fn process_compacted_history_inserts_context_before_last_real_user_message_only() {
    let compacted_history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "older user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsummary text"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "latest user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let (refreshed, initial_context) = process_compacted_history_with_test_session(
        compacted_history,
        /*previous_turn_settings*/ None,
    )
    .await;
    assert_eq!(refreshed, initial_context);
}

#[tokio::test]
async fn process_compacted_history_reinjects_model_switch_message() {
    let compacted_history = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let previous_turn_settings = PreviousTurnSettings {
        model: "previous-regular-model".to_string(),
        comp_hash: None,
    };

    let (refreshed, initial_context) = process_compacted_history_with_test_session(
        compacted_history,
        Some(&previous_turn_settings),
    )
    .await;

    let ResponseItem::Message { role, content, .. } = &initial_context[0] else {
        panic!("expected developer message");
    };
    assert_eq!(role, "developer");
    let [ContentItem::InputText { text }, ..] = content.as_slice() else {
        panic!("expected developer text");
    };
    assert!(text.contains("<model_switch>"));

    assert_eq!(refreshed, initial_context);
}

#[test]
fn insert_initial_context_before_last_real_user_or_summary_keeps_summary_last() {
    let compacted_history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "older user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "latest user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsummary text"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let initial_context = vec![ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "fresh permissions".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let refreshed =
        insert_initial_context_before_last_real_user_or_summary(compacted_history, initial_context);
    let expected = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "older user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "fresh permissions".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "latest user".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsummary text"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(refreshed, expected);
}

#[test]
fn insert_initial_context_before_last_real_user_or_summary_keeps_compaction_last() {
    let compacted_history = vec![ResponseItem::Compaction {
        id: None,
        encrypted_content: "encrypted".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }];
    let initial_context = vec![ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "fresh permissions".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let refreshed =
        insert_initial_context_before_last_real_user_or_summary(compacted_history, initial_context);
    let expected = vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "fresh permissions".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "encrypted".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(refreshed, expected);
}
