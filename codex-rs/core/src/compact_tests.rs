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

#[tokio::test]
async fn retained_active_plan_omits_retired_history_without_mutating_owner() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let plan = codex_protocol::plan_tool::UpdatePlanArgs {
        explanation: None,
        plan: vec![codex_protocol::plan_tool::PlanItemArg {
            step: "current".into(), status: codex_protocol::plan_tool::StepStatus::Pending,
        }],
    };
    let mut lineage = crate::plan_store::PlanLineage::default();
    for index in 0..40 {
        lineage.requirements.insert(format!("retired-{index}"), crate::plan_store::PlanRequirement {
            text: format!("historical detail {index}"),
            status: codex_protocol::plan_tool::StepStatus::Completed, superseded_reason: None,
        });
    }
    lineage.requirements.insert("orphan".into(), crate::plan_store::PlanRequirement {
        text: "unresolved recovery work".into(),
        status: codex_protocol::plan_tool::StepStatus::Pending, superseded_reason: None,
    });
    session.services.plan_store.restore_with_lineage(Some(plan), Some(lineage)).await;
    let before = session.services.plan_store.snapshot_with_lineage().await;
    let item = retained_plan_context(&session).await.unwrap().unwrap();
    let text = serde_json::to_string(&item).unwrap();
    assert!(!text.contains("historical detail"));
    assert!(text.contains("unresolved recovery work"));
    assert_eq!(session.services.plan_store.snapshot_with_lineage().await, before);
}

#[test]
fn omission_selectors_resolve_interleaved_messages_in_one_selection() {
    let source = vec![
        user_message(&"first exact constraint ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS * 3)),
        ResponseItem::FunctionCall { id:None, name:"inspect".into(), namespace:None,
            arguments:"{}".into(), call_id:"tool".into(), internal_chat_message_metadata_passthrough:None },
        ResponseItem::FunctionCallOutput { id:None, call_id:"tool".into(),
            output:codex_protocol::models::FunctionCallOutputPayload::from_text("evidence".into()),
            internal_chat_message_metadata_passthrough:None },
        user_message(&"latest exact constraint ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS * 3)),
    ];
    let (retained, _, _, _, omitted) = build_bounded_input_history(source.clone(), false);
    assert!(omitted);
    let canonical = compaction_text_recovery_for_items(source.clone());
    let mut checked = 0;
    for item in retained {
        let ResponseItem::Message { content, .. } = item else { continue; };
        for part in content {
            let ContentItem::InputText { text } = part else { continue; };
            let Ok(receipt) = serde_json::from_str::<serde_json::Value>(&text) else { continue; };
            if receipt["kind"] != COMPACT_TEXT_OMISSION_MARKER { continue; }
            let index = receipt["source_index"].as_u64().unwrap() as usize;
            let pointer = receipt["recovery_selector"]["pointer"].as_str().unwrap();
            assert_eq!(canonical.value.as_ref().unwrap().pointer(pointer).unwrap(), &serde_json::to_value(&source[index]).unwrap());
            checked += 1;
        }
    }
    assert_eq!(checked, 2);
}

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

fn assert_regenerated_initial_context(actual: &[ResponseItem], mut expected: Vec<ResponseItem>) {
    assert!(!expected.is_empty(), "initial context must be injected");
    assert_eq!(actual.len(), expected.len());
    let mut generated_ids = std::collections::HashSet::new();
    for (actual, expected) in actual.iter().zip(&mut expected) {
        let (
            ResponseItem::Message {
                id: Some(actual_id),
                ..
            },
            ResponseItem::Message {
                id: Some(expected_id),
                ..
            },
        ) = (actual, expected)
        else {
            panic!("initial context must contain messages with trusted provenance IDs");
        };
        for id in [&*expected_id, actual_id] {
            assert!(id.as_str().starts_with("msg_sctx_"));
            assert!(
                generated_ids.insert(id.to_string()),
                "context IDs must be unique"
            );
        }
        // Independent renders generate fresh IDs. Compare every other field exactly.
        *expected_id = actual_id.clone();
    }
    assert_eq!(actual, expected.as_slice());
}

#[tokio::test]
async fn compaction_reuses_a_generation_workspace_identity_without_recapturing_git() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let prefetched = session
        .services
        .git_workspace
        .workspace_evidence_identity(turn_context.config.cwd.as_path())
        .await;
    let captures_before = session
        .services
        .git_workspace
        .workspace_evidence_capture_count();

    let resolved =
        workspace_identity_for_compaction(&session, &turn_context, Some(&prefetched)).await;

    assert_eq!(resolved, prefetched);
    assert_eq!(
        session
            .services
            .git_workspace
            .workspace_evidence_capture_count(),
        captures_before,
        "mid-turn compaction must reuse the generation-boundary identity"
    );
}

#[test]
fn inline_compaction_reuses_the_supplied_client_session_without_creating_one() {
    let mut supplied_session = 17_u8;
    let mut create_called = false;

    {
        let mut selected =
            reuse_or_create_compaction_client_session(Some(&mut supplied_session), || {
                create_called = true;
                99
            });
        *selected.as_mut() = 23;
    }

    assert!(!create_called);
    assert_eq!(supplied_session, 23);
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
        json!({"large_0": {"value": 0}, "_codex_extension_delivery": {
            "large_0": {"role": "developer", "text": "a".repeat(30_000)}
        }})
    );
    let (retry, final_snapshot) = world_state.render_diff_with_snapshot(&delivered_snapshot);
    assert_eq!(retry.len(), 1);
    assert_eq!(final_snapshot.into_value(), json!({
        "large_0": {"value": 0}, "large_1": {"value": 1},
        "_codex_extension_delivery": {
            "large_0": {"role": "developer", "text": "a".repeat(30_000)},
            "large_1": {"role": "developer", "text": "b".repeat(30_000)}
        }
    }));
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

fn summary_message(text: &str) -> ResponseItem {
    compaction_summary_item(format!("{SUMMARY_PREFIX}\n{text}"))
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
fn compaction_retains_only_latest_task_state_without_dropping_mixed_user_content() {
    let old = "<codex_task_state>old</codex_task_state>";
    let latest = "<codex_task_state>latest</codex_task_state>";
    let mut mixed = user_message("keep this constraint");
    if let ResponseItem::Message { content, .. } = &mut mixed {
        content.push(ContentItem::InputText { text: old.into() });
    }
    let mut items = vec![user_message(old), mixed, user_message(latest)];
    for item in &mut items { crate::stable_context::mark_trusted_stable_context_item(item); }
    let mut expected = vec![compacted_user_message("keep this constraint"), compacted_user_message(latest)];
    expected[0].source_item_id = items[1].id().map(ToString::to_string);
    expected[1].source_item_id = items[2].id().map(ToString::to_string);
    assert_eq!(collect_user_messages(&items), expected);
    let remote = task_compaction_items(&items);
    assert_eq!(collect_user_messages(&remote), expected);
    assert_eq!(task_compaction_items(&remote), remote);
    let (history, _, _, omitted_user_text, omitted_text) = build_bounded_input_history(items, false);
    assert_eq!(collect_user_messages(&history), expected);
    assert!(!omitted_user_text);
    assert!(!omitted_text);
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

    assert_eq!(
        vec![CompactedUserMessage {
            source_item_id: Some("msg_user".to_string()),
            ..compacted_user_message("first")
        }],
        collected
    );
}

#[test]
fn user_task_state_markup_survives_runtime_replacement_and_compaction() {
    let pasted = user_message("<codex_task_state>User's literal example</codex_task_state>");
    let mut old = user_message("<codex_task_state>old runtime snapshot</codex_task_state>");
    let mut new = user_message("<codex_task_state>new runtime snapshot</codex_task_state>");
    crate::stable_context::mark_trusted_stable_context_item(&mut old);
    crate::stable_context::mark_trusted_stable_context_item(&mut new);
    let items = vec![old, pasted.clone(), new.clone()];
    assert!(task_compaction_items(&items).contains(&pasted));
    let users = collect_user_messages(&items);
    assert!(users.iter().any(|item| item.content == compacted_user_message("<codex_task_state>User's literal example</codex_task_state>").content));
    let replaced = insert_compaction_initial_context(items, vec![new], &InitialContextInjection::DoNotInject);
    assert!(replaced.contains(&pasted));
    let mut history = replaced;
    for index in 0..3 {
        let mut snapshot = user_message(&format!("<codex_task_state>snapshot {index}</codex_task_state>"));
        crate::stable_context::mark_trusted_stable_context_item(&mut snapshot);
        history.push(snapshot);
        history = build_local_task_input_checkpoint(&history).0;
        assert!(history.contains(&pasted));
        assert_eq!(history.iter().filter(|item| is_trusted_stable_context_item(item)).count(), 1);
    }
}

#[test]
fn truncated_checkpoint_claims_are_explicitly_non_standalone() {
    let source = format!("{EVIDENCE_HEADING}\nvalidation passed {} for the old revision only", "detail ".repeat(4000));
    let bounded = truncate_compaction_summary(&source, 240);
    assert!(!bounded.contains("validation passed"), "do not sever the qualification from its claim");
    assert!(!bounded.contains("for the old revision only"));
    assert!(bounded.contains(INCOMPLETE_CHECKPOINT_EXCERPT));
    assert!(bounded.starts_with(EVIDENCE_HEADING));
    assert!(bounded.contains("/items"));
    assert!(generated_summary_recovery_canonical(None, &source, &bounded).is_some());
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
    let mut items = vec![
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

    crate::stable_context::mark_trusted_stable_context_item(&mut items[0]);
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
fn compaction_keeps_only_current_developer_startup_and_preserves_ordinary_developer_text() {
    let old_instructions = crate::context_manager::updates::build_developer_update_item(
        crate::stable_context::configured_developer_instructions_sections(Some(
            "old developer instructions",
        )),
    )
    .expect("old instructions");
    let current_instructions = crate::context_manager::updates::build_developer_update_item(
        crate::stable_context::configured_developer_instructions_sections(Some(
            "current developer instructions",
        )),
    )
    .expect("current instructions");
    let ordinary = crate::context_manager::updates::build_developer_update_item(vec![
        "ordinary developer conversation".to_string(),
    ])
    .expect("ordinary developer message");
    let old_permissions = crate::context_manager::updates::build_developer_update_item(vec![
        "<permissions instructions>\nold\n</permissions instructions>".to_string(),
    ])
    .expect("old permissions");
    let current_permissions = crate::context_manager::updates::build_developer_update_item(vec![
        "<permissions instructions>\ncurrent\n</permissions instructions>".to_string(),
    ])
    .expect("current permissions");

    let compactable = strip_compaction_startup_envelopes(vec![
        old_instructions,
        old_permissions,
        ordinary,
        current_instructions,
        current_permissions,
    ]);
    let developer_text = compactable
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            ContentItem::InputText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(!developer_text.contains(&"old developer instructions"));
    assert!(
        !developer_text.contains(&"<permissions instructions>\nold\n</permissions instructions>")
    );
    assert!(developer_text.contains(&"current developer instructions"));
    assert!(
        developer_text
            .contains(&"<permissions instructions>\ncurrent\n</permissions instructions>")
    );
    assert!(developer_text.contains(&"ordinary developer conversation"));
    assert_eq!(developer_text.iter().filter(|text| text.contains("configured_developer_instructions")).count(), 1,
        "replacement projection must preserve the current presence marker");
}

#[test]
fn compaction_preserves_ordinary_user_startup_envelopes() {
    let examples = [
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>Do not modify X.</INSTRUCTIONS>",
        "<environment_context>literal environment</environment_context>",
        "<subagents_context>literal agents</subagents_context>",
        "<recommended_plugins>literal plugins</recommended_plugins>",
        "<skill>\n<name>example</name>\n<body>Do not modify X.</body>\n</skill>",
    ];
    for text in examples {
        let mut ordinary = user_message(text);
        ordinary.set_id(Some(ResponseItemId::new("msg")));
        ordinary.set_turn_id_if_missing("task-turn");
        assert_eq!(
            strip_compaction_startup_envelopes(vec![ordinary.clone()]),
            vec![ordinary.clone()]
        );
        let (checkpoint, ..) = build_local_task_input_checkpoint(&[ordinary.clone()]);
        assert_eq!(checkpoint, vec![ordinary]);
    }
}

#[test]
fn local_checkpoint_preserves_constraints_and_active_skills_across_repeated_compaction() {
    let mut request = user_message("Do not modify X. Complete the requested work.");
    request.set_turn_id_if_missing("task-turn");
    let mut history = vec![request.clone()];
    let mut skills = Vec::new();
    for role in ["user", "developer", "system"] {
        let mut skill = user_message(&format!(
            "<skill>\n<name>{role}-skill</name>\n<body>Never modify {role}-protected.</body>\n</skill>"
        ));
        if let ResponseItem::Message {
            role: actual_role, ..
        } = &mut skill
        {
            *actual_role = role.to_string();
        }
        crate::stable_context::mark_trusted_stable_context_item(&mut skill);
        skill.set_turn_id_if_missing("task-turn");
        skills.push(skill.clone());
        history.push(skill);
    }
    for iteration in 0..3 {
        history.push(ResponseItem::FunctionCall {
            id: None,
            name: "exec".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: format!("call-{iteration}"),
            internal_chat_message_metadata_passthrough: None,
        });
        let (mut checkpoint, images, omitted_images, omitted_user, omitted_text) =
            build_local_task_input_checkpoint(&history);
        assert_eq!(
            (images, omitted_images, omitted_user, omitted_text),
            (0, 0, false, false)
        );
        assert_eq!(checkpoint[0], request);
        for skill in &skills {
            assert_eq!(checkpoint.iter().filter(|item| *item == skill).count(), 1);
        }
        let (remote_checkpoint, _, _) = build_task_input_checkpoint(&history);
        for skill in &skills {
            assert_eq!(
                remote_checkpoint
                    .iter()
                    .filter(|item| *item == skill)
                    .count(),
                1
            );
        }
        checkpoint.push(compaction_context_message(
            "<codex_internal_context source=\"compaction_plan\">Retained checklist, not a new request.</codex_internal_context>".to_string(),
        ));
        let mut summary = summary_message("A lossy handoff without the prohibitions.");
        summary.set_turn_id_if_missing(&format!("compact-{iteration}"));
        checkpoint.push(summary);
        let sampled = crate::stable_context::project_stable_context(
            checkpoint.clone().into(),
            crate::stable_context::StableContextTarget::Sampling,
        );
        for skill in &skills {
            let ResponseItem::Message { role, content, .. } = skill else {
                unreachable!()
            };
            assert_eq!(
                sampled
                    .items
                    .iter()
                    .filter(|item| matches!(item,
                        ResponseItem::Message { role: actual_role, content: actual_content, .. }
                            if actual_role == role && actual_content == content
                    ))
                    .count(),
                1
            );
        }
        history = checkpoint;
    }
    let mut next_request = user_message("A different task, without selected skills.");
    next_request.set_turn_id_if_missing("next-task");
    history.push(next_request);
    let (checkpoint, ..) = build_local_task_input_checkpoint(&history);
    assert!(checkpoint.iter().any(is_selected_skill_item),
        "a new turn alone is not accepted task-completion evidence");
}

#[test]
fn local_checkpoint_omission_keeps_exact_task_recovery() {
    let request = user_message(&format!(
        "start {} Do not modify X. {} end",
        "before ".repeat(20_000),
        "after ".repeat(20_000)
    ));
    let history = vec![request.clone(), summary_message("lossy summary")];
    let (checkpoint, _, _, omitted_user, omitted_text) =
        build_local_task_input_checkpoint(&history);
    assert!(omitted_user && omitted_text);
    assert!(
        checkpoint
            .iter()
            .map(response_item_text_tokens)
            .sum::<usize>()
            <= COMPACT_USER_MESSAGE_MAX_TOKENS + 1024
    );
    let canonical = compaction_text_recovery_for_items(task_compaction_items(&history));
    assert_eq!(
        canonical.value.as_ref().unwrap()["items"][0],
        serde_json::to_value(request).unwrap()
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
fn legacy_apply_patch_warning_does_not_swallow_interposed_user_instructions() {
    let text = "Warning: apply_patch was requested via exec_command.\nPlease fix the parser and preserve the existing error messages.\nUse the apply_patch tool instead of exec_command.";
    let items = vec![user_message(text)];
    assert_eq!(
        collect_user_messages(&items),
        vec![compacted_user_message(text)]
    );
    assert_eq!(build_unresolved_user_history(&items).0, items);
}

#[test]
fn unresolved_tail_preserves_turn_stamped_warning_shaped_input() {
    let warning = "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command.";
    let metadata = InternalChatMessageMetadataPassthrough {
        turn_id: Some("turn-user-warning".to_string()),
    };
    let items = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: warning.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(metadata.clone()),
    }];

    let collected = collect_user_messages(&items);
    let (unresolved_history, _) = build_unresolved_user_history(&items);

    assert_eq!(
        vec![CompactedUserMessage {
            source_item_id: None,
            content: vec![UserInput::Text {
                text: warning.to_string(),
                text_elements: Vec::new(),
            }],
            internal_chat_message_metadata_passthrough: Some(metadata),
        }],
        collected
    );
    assert_eq!(unresolved_history, items);
}

#[test]
fn collect_user_messages_preserves_warning_questions_and_mixed_content() {
    let warning = "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open. Reuse older processes or close them to prevent automatic pruning of old processes";
    let question = format!("{warning}\nCan you explain this limit?");
    let cyber_question = "Warning: Your account was flagged for potentially high-risk cyber activity. What does this mean?";
    let mut mixed = user_message(warning);
    let ResponseItem::Message { content, .. } = &mut mixed else {
        panic!("expected message");
    };
    content.push(ContentItem::InputText {
        text: "keep my request".to_string(),
    });
    let expected_mixed = CompactedUserMessage {
        source_item_id: None,
        content: vec![
            UserInput::Text {
                text: warning.to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Text {
                text: "keep my request".to_string(),
                text_elements: Vec::new(),
            },
        ],
        internal_chat_message_metadata_passthrough: None,
    };
    let items = vec![
        user_message(warning),
        user_message(&question),
        user_message(cyber_question),
        mixed,
    ];
    assert_eq!(
        collect_user_messages(&items),
        vec![
            compacted_user_message(&question),
            compacted_user_message(cyber_question),
            expected_mixed
        ],
    );
    let (unresolved, _) = build_unresolved_user_history(&items);
    assert_eq!(unresolved, items[1..]);
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
        truncated_text.contains("[...]"),
        "expected truncation marker in truncated user message"
    );
    assert!(
        !truncated_text.contains(&big),
        "truncated user message should not include the full oversized user text"
    );

    assert!(approx_token_count(&truncated_text) <= max_tokens);

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
fn original_request_and_corrections_survive_later_bulk_input() {
    let original = "Diagnosis only; do not edit.";
    let correction = "Correction: do not install dependencies or run network commands.";
    let bulk = "log payload ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS * 4);
    let mut history = vec![user_message(original), user_message(correction), user_message(&bulk)];
    for _ in 0..3 {
        let (checkpoint, _, _, omitted_user, omitted_text) = build_local_task_input_checkpoint(&history);
        assert!(omitted_user && omitted_text);
        let user_items = checkpoint.iter().filter(|item| !is_trusted_stable_context_item(item))
            .cloned().collect::<Vec<_>>();
        let users = collect_user_messages(&user_items);
        assert!(users.iter().any(|message| message.content == compacted_user_message(original).content));
        assert!(users.iter().any(|message| message.content == compacted_user_message(correction).content));
        assert!(users.iter().map(compacted_user_message_text_tokens).sum::<usize>() <= COMPACT_USER_MESSAGE_MAX_TOKENS);
        history = checkpoint;
        history.push(user_message(&bulk));
    }
}

#[test]
fn rebase_cannot_silently_erase_unresolved_prohibition() {
    let anchor = "Diagnosis only; do not edit. Verification remains unfinished.";
    let previous = format!("{SUMMARY_PREFIX}\n## Goal\nDiagnose.\n## Current state\nInvestigating.\n## Completed work\nRead source.\n## Unresolved work\n{anchor}\n## Evidence\nSource.\n## Next action\nVerify.");
    let error = validated_rebased_compaction_summary(&previous, "## Unresolved work\nNone", &[3]).unwrap_err();
    assert!(error.to_string().contains("omitted a prior unresolved anchor"));
    let accounted = format!("## Unresolved work\nNone\n## Completed work\nResolved: {anchor} Evidence: diagnosis delivered without edits.");
    let summary = validated_rebased_compaction_summary(&previous, &accounted, &[3]).unwrap();
    assert!(summary.contains(anchor), "accounting remains a visible model claim, not proof");
}

#[test]
fn incremental_compaction_preserves_the_previous_summary_prefix() {
    let previous = format!("{SUMMARY_PREFIX}\nverified state");
    let summary = bounded_task_state_summary(Some(&previous), "new unresolved item");

    assert!(summary.starts_with(&previous));
    assert_eq!(summary, format!("{previous}\n\nnew unresolved item"));
}

#[test]
fn incremental_rebases_preserve_old_unresolved_work_through_unrelated_updates() {
    let mut summary = format!("{SUMMARY_PREFIX}\n## Goal\nInvestigate.\n## Current state\nStarted.\n## Completed work\nRead source.\n## Unresolved work\nOLD-OBLIGATION remains unresolved.\n## Evidence\nOriginal source.\n## Next action\nVerify.");
    let mut rebases = 0;
    for index in 0..100 {
        let sections = compaction_rebase_sections(&summary);
        let mut suffix = format!("## Evidence\nObservation {index}: {}", "unrelated evidence ".repeat(30));
        if sections.contains(&4) {
            rebases += 1;
            assert!(compaction_rebase_prompt(&sections).contains("complete refreshed state"));
            suffix = format!("## Evidence\nCurrent observation {index}; older observations superseded.");
        }
        // Next action is always refreshed by the existing rebase policy.
        suffix.push_str("\n## Next action\nVerify.");
        summary = validated_rebased_compaction_summary(&summary, &suffix, &sections).unwrap();
        assert!(summary.contains("OLD-OBLIGATION remains unresolved."));
        assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    }
    assert!(rebases > 5);
    let refreshed = "## Unresolved work\nOLD-OBLIGATION remains unresolved. New obligation added.";
    let summary = validated_rebased_compaction_summary(&summary, refreshed, &[3]).unwrap();
    assert_eq!(summary.matches("OLD-OBLIGATION").count(), 1);
    assert!(validated_rebased_compaction_summary(&summary, "## Evidence\nNew fact", &[3]).is_err());
    assert!(validated_rebased_compaction_summary(&summary,
        &format!("## Evidence\n{}", "overflow ".repeat(10_000)), &[]).is_err());
}

#[test]
fn incremental_next_action_is_always_a_single_replacement() {
    let mut summary = format!("{SUMMARY_PREFIX}\n## Goal\nFix.\n## Current state\nWorking.\n## Completed work\nNone.\n## Unresolved work\nDo not modify X.\n## Evidence\nExact user request.\n## Next action\nInspect.");
    for action in ["Edit permitted files.", "Validate.", "Deliver."] {
        let sections = compaction_rebase_sections(&summary);
        assert!(sections.contains(&5));
        let suffix = format!("## Next action\n{action}");
        summary = validated_rebased_compaction_summary(&summary, &suffix, &sections).unwrap();
        assert_eq!(summary.matches(NEXT_ACTION_HEADING).count(), 1);
        assert!(summary.ends_with(action));
        assert!(summary.contains("Do not modify X."));
    }
}

#[test]
fn short_corrections_following_bulk_payloads_keep_exact_budget_priority() {
    let messages = vec![
        compacted_user_message(&"bulk ".repeat(10_000)),
        compacted_user_message("Do not modify X."),
        compacted_user_message("Also inspect Y."),
    ];
    let (history, _, _, indices) = append_bounded_user_messages(Vec::new(), &messages, 100, 0, 0);
    assert_eq!(indices, vec![0, 1, 2]);
    assert_eq!(collect_user_messages(&history)[1..], messages[1..]);
    assert!(collect_user_messages(&history).iter().map(compacted_user_message_text_tokens).sum::<usize>() <= 100);
}

#[test]
fn semantic_summary_truncation_preserves_conversation_state() {
    let intent = "INTENT-SENTINEL";
    let unresolved = "UNRESOLVED-SENTINEL";
    let generated = format!(
        "{GOAL_HEADING}\n{intent}\n\n{}\n\n{CURRENT_STATE_HEADING}\nworking\n\n{COMPLETED_WORK_HEADING}\ndone\n\n{UNRESOLVED_WORK_HEADING}\n{unresolved}\n\n{}\n\n{EVIDENCE_HEADING}\nverified\n\n{NEXT_ACTION_HEADING}\ncontinue",
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
    for ((heading, _), populated) in COMPACTION_SECTIONS
        .iter()
        .zip(compaction_section_bodies(&summary))
    {
        assert!(populated, "bounded checkpoint lost {heading}: {summary}");
    }
    assert!(summary.contains(INCOMPLETE_CHECKPOINT_EXCERPT));
    assert!(validate_generated_compaction_summary(None, &summary).is_err());
    assert!(generated_summary_recovery_canonical(None, &generated, &summary).is_some());
}

#[test]
fn first_and_custom_compaction_prioritize_unresolved_over_completed_text() {
    let unresolved = format!("{}\nMIDDLE-UNRESOLVED-CONSTRAINT\n{}", "pending detail ".repeat(150), "pending detail ".repeat(150));
    let generated = format!(
        "{}\n{GOAL_HEADING}\nfinish\n{CURRENT_STATE_HEADING}\nactive\n{COMPLETED_WORK_HEADING}\n{}\n{UNRESOLVED_WORK_HEADING}\n{unresolved}\n{EVIDENCE_HEADING}\nobserved\n{NEXT_ACTION_HEADING}\ncheck",
        "intro ".repeat(4_000), "completed ".repeat(8_000),
    );
    for custom in [false, true] {
        let bounded = validated_compaction_summary(None, &generated, true, custom).unwrap();
        assert!(bounded.contains(unresolved.trim()), "unresolved work must remain actively visible");
        assert!(approx_token_count(&bounded) <= COMPACT_TASK_STATE_MAX_TOKENS);
        assert!(generated_summary_recovery_canonical(None, &generated, &bounded).is_some());
    }
}

#[test]
fn compaction_validation_accepts_a_later_nonempty_duplicate_section() {
    let checkpoint = format!(
        "{GOAL_HEADING}\n\n{CURRENT_STATE_HEADING}\nworking\n\n{GOAL_HEADING}\nkeep the user requirement\n\n{COMPLETED_WORK_HEADING}\nverified\n\n{UNRESOLVED_WORK_HEADING}\nnone\n\n{EVIDENCE_HEADING}\ntest passed\n\n{NEXT_ACTION_HEADING}\nfinish"
    );
    let summary = validated_compaction_summary(None, &checkpoint, true, false)
        .expect("a complete checkpoint remains valid when a heading was repeated");
    assert!(summary.contains("keep the user requirement"));
    assert!(summary.contains("test passed"));
    let incomplete = checkpoint.replace("keep the user requirement", "");
    let error = validated_compaction_summary(None, &incomplete, true, false).unwrap_err();
    assert!(error.to_string().contains(GOAL_HEADING));
}

#[test]
fn compaction_section_allocations_preserve_evidence_and_next_action_under_pressure() {
    let evidence_sentinel = "EVIDENCE-RETENTION-SENTINEL";
    let next_action_sentinel = "NEXT-ACTION-RETENTION-SENTINEL";
    let generated = format!(
        "{}\n{}",
        "preamble ".repeat(1_000),
        COMPACTION_SECTIONS
            .iter()
            .map(|(heading, _)| {
                let sentinel = match *heading {
                    EVIDENCE_HEADING => evidence_sentinel,
                    NEXT_ACTION_HEADING => next_action_sentinel,
                    _ => "SECTION-RETENTION-SENTINEL",
                };
                format!(
                    "{heading}\n{sentinel}\n\n{}",
                    "bounded content ".repeat(1_000)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    );

    assert!(
        approx_token_count(&generated) > COMPACT_TASK_STATE_MAX_TOKENS,
        "the fixture must require truncation even at the larger budget"
    );
    let undersized_summary = truncate_compaction_summary(&generated, 1_800);
    assert!(approx_token_count(&undersized_summary) <= 1_800);

    let summary = truncate_compaction_summary(&generated, COMPACT_TASK_STATE_MAX_TOKENS);

    assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(
        summary.contains(evidence_sentinel),
        "bounded checkpoint lost meaningful Evidence content: {summary}"
    );
    assert!(
        summary.contains(next_action_sentinel),
        "bounded checkpoint lost meaningful Next action content: {summary}"
    );
}

#[test]
fn structured_compaction_summary_respects_feasible_token_budgets() {
    let source = format!(
        "{}\n{}",
        "preamble ".repeat(400),
        COMPACTION_SECTIONS
            .iter()
            .map(|(heading, _)| {
                format!("{heading}\n{}", r#"{"a":1,"b":2} 漢字🦀 "#.repeat(100))
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    // Structured handoffs retain all six headings and a body for each section;
    // budgets below that irreducible minimum intentionally preserve structure.
    for max_tokens in [100, 300, 1_800, 2_400] {
        let result = truncate_compaction_summary(&source, max_tokens);
        assert!(
            approx_token_count(&result) <= max_tokens,
            "budget {max_tokens}: {result}"
        );
        for (heading, _) in COMPACTION_SECTIONS {
            assert!(result.lines().any(|line| line == heading), "{result}");
        }
    }
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
fn generated_compaction_requires_structure_except_explicit_custom_handoffs() {
    let complete = format!(
        "{GOAL_HEADING}\nfinish recovery\n\n{CURRENT_STATE_HEADING}\nimplementation present\n\n{COMPLETED_WORK_HEADING}\nproducer updated\n\n{UNRESOLVED_WORK_HEADING}\nkeep ambiguity\n\n{EVIDENCE_HEADING}\nfocused evidence\n\n{NEXT_ACTION_HEADING}\nrun focused proof"
    );
    assert!(validate_generated_compaction_summary(None, &complete).is_ok());

    let incomplete = format!(
        "{GOAL_HEADING}\nfinish recovery\n\n{CURRENT_STATE_HEADING}\nimplementation present\n\n{COMPLETED_WORK_HEADING}\nproducer updated\n\n{UNRESOLVED_WORK_HEADING}\nkeep ambiguity\n\n{EVIDENCE_HEADING}\nfocused evidence\n\n{NEXT_ACTION_HEADING}\n"
    );
    assert!(validate_generated_compaction_summary(None, &incomplete).is_err());
    assert!(validate_generated_compaction_summary(None, "Done").is_err());
    assert!(validate_generated_compaction_summary(Some(&complete), "free-form update").is_err());
    assert!(validated_compaction_summary(None, "custom free-form checkpoint", true, true).is_ok());
    assert!(validated_compaction_summary(None, "", true, true).is_err());
    assert!(validated_compaction_summary(None, &incomplete, true, true).is_err());
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
        summary_message("prior checkpoint"),
        user_message(&"unresolved constraint ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS * 3)),
    ];

    let (_, _, _, omitted_user_text, _) = build_bounded_unresolved_input_history(&items);

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

    let (history, _, _, omitted_user_text, _) = build_bounded_unresolved_input_history(&items);
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

    let (history, _, _, omitted_user_text, _) = build_bounded_unresolved_input_history(&items);
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
fn over_truncation_moderate_unresolved_user_text_is_retained_without_a_retry() {
    let sentinel = "ROOT_CONSTRAINT_SENTINEL";
    let text = format!(
        "{}{}{}",
        "exact ".repeat(2_000),
        sentinel,
        " tail".repeat(2_000)
    );
    assert!(approx_token_count(&text) > 4_000);
    assert!(approx_token_count(&text) < COMPACT_USER_MESSAGE_MAX_TOKENS);

    let (history, _, _, omitted_user_text, _) =
        build_bounded_unresolved_input_history(&[user_message(&text)]);
    let rendered = serde_json::to_string(&history).expect("history serializes");

    assert!(!omitted_user_text);
    assert!(rendered.contains(sentinel));
    assert!(!rendered.contains(COMPACT_TEXT_OMISSION_MARKER));
}

#[test]
fn over_truncation_large_unresolved_text_gets_exact_artifact_recovery_payload() {
    let sentinel = "CENTRAL_EXACT_CONSTRAINT_SENTINEL";
    let text = format!(
        "{}{}{}",
        "leading constraint ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS),
        sentinel,
        " trailing constraint".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS)
    );
    let items = vec![user_message(&text)];
    let (_, _, _, omitted_user_text, omitted_text) = build_bounded_unresolved_input_history(&items);

    assert!(omitted_user_text);
    let canonical = compaction_text_recovery_canonical(&items, omitted_text)
        .expect("omitted unresolved text must get a canonical recovery payload");
    assert!(String::from_utf8_lossy(&canonical.bytes).contains(sentinel));
    assert_eq!(
        canonical.value.as_ref().unwrap()["kind"],
        "local_compaction_text_recovery"
    );
    assert!(canonical.complete);
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

    let (history, _, _, omitted_user_text, omitted_text) =
        build_bounded_unresolved_input_history(&items);
    let rendered = serde_json::to_string(&history).expect("history serializes");

    assert!(rendered.contains(COMPACT_TEXT_OMISSION_MARKER));
    let receipt = history
        .iter()
        .find_map(|item| match item {
            ResponseItem::Message { content, .. } => content.iter().find_map(|content| {
                let ContentItem::InputText { text } = content else {
                    return None;
                };
                let value: serde_json::Value = serde_json::from_str(text).ok()?;
                (value["kind"] == COMPACT_TEXT_OMISSION_MARKER).then_some(value)
            }),
            _ => None,
        })
        .expect("agent omission receipt");
    assert_eq!(receipt["role"], "agent");
    assert!(receipt["omitted_tokens"].as_u64().unwrap() > 0);
    assert!(!omitted_user_text);
    let canonical = compaction_text_recovery_canonical(&items, omitted_text)
        .expect("agent text omissions also need exact recovery");
    assert_eq!(
        canonical.value.as_ref().unwrap()["items"],
        json!([items[1]])
    );
}

#[test]
fn task_checkpoint_bounds_handoff_and_preserves_exact_recovery_text() {
    let request =
        user_message("Implement model, tool, network, IPC, database, and subprocess handling.");
    let text = format!(
        "Open acceptance requirements:\n{}\nNetwork handling is unfinished.",
        "evidence ".repeat(8_000)
    );
    let handoff = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText { text: text.clone() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let input = vec![request.clone(), handoff];
    let (checkpoint, _, omitted_text) = build_task_input_checkpoint(&input);
    assert!(omitted_text);
    assert_eq!(checkpoint[0], request);
    assert!(response_item_text_tokens(&checkpoint[1]) <= COMPACT_TASK_STATE_MAX_TOKENS);
    let canonical = compaction_text_recovery_for_items(task_compaction_items(&input));
    assert_eq!(
        canonical.value.as_ref().unwrap()["items"][1]["content"][0]["text"],
        text
    );
}

#[test]
fn task_checkpoint_drops_superseded_artifact_pin_sets() {
    let request = user_message("Keep going on the parser.");
    let pins = user_message(
        &json!({
            "version": 1,
            "kind": "tool_history_artifact_pins",
            "artifacts": [{"artifact_id": "a1", "call_id": "c1"}],
        })
        .to_string(),
    );
    let later_request = user_message("Now run the tests.");
    let (checkpoint, _, _) =
        build_task_input_checkpoint(&[request.clone(), pins.clone(), later_request.clone()]);
    assert_eq!(checkpoint, vec![request, later_request]);
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

    let (history, _, _, _, _) = build_bounded_unresolved_input_history(&items);

    assert_eq!(history, vec![call, output]);
    assert_eq!(compaction_summary_items(&items), vec![user_message("request")]);
}

#[test]
fn compaction_summary_excludes_unread_tail_but_keeps_consumed_evidence() {
    let call = |id: &str| ResponseItem::FunctionCall {
        id: None, name: "inspect".into(), namespace: None, arguments: "{}".into(),
        call_id: id.into(), internal_chat_message_metadata_passthrough: None,
    };
    let output = |id: &str| ResponseItem::FunctionCallOutput {
        id: None, call_id: id.into(),
        output: codex_protocol::models::FunctionCallOutputPayload::from_text("evidence".into()),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut assistant = user_message("interpreted earlier evidence");
    if let ResponseItem::Message { role, .. } = &mut assistant { *role = "assistant".into(); }
    let consumed = vec![user_message("request"), call("done"), output("done"), assistant];
    let mut items = consumed.clone();
    items.extend([call("pending-a"), call("pending-b"), output("pending-a"),
        output("pending-b"), user_message("new constraint"), agent_message("unread agent result")]);
    assert_eq!(compaction_summary_items(&items), consumed);
    let (retained, _, _, _, _) = build_bounded_unresolved_input_history(&items);
    assert_eq!(retained, items[consumed.len()..]);
    assert!(compaction_summary_items(&[user_message("first request")]).is_empty());
    let summary = compaction_summary_item(format!("{SUMMARY_PREFIX}\nprevious handoff"));
    assert_eq!(compaction_summary_items(&[summary.clone(), user_message("new request")]),
        vec![summary]);

    let mut history = crate::context_manager::ContextManager::new();
    history.replace(compaction_summary_items(&items));
    let prompt = history.for_compaction_prompt_with_completed_tool_projection(
        &codex_protocol::openai_models::default_input_modalities(), None,
    );
    let serialized = serde_json::to_string(&prompt).unwrap();
    assert!(!serialized.contains("pending-a"));
    assert!(!serialized.contains("pending-b"));
    assert!(!serialized.contains("new constraint"));
    assert!(serialized.contains("interpreted earlier evidence"));
}

#[test]
fn newest_section_updates_include_separator_cost_in_their_budget() {
    let updates = (0..200)
        .map(|index| {
            let update = format!("update-{index}");
            let tokens = approx_token_count(&update);
            (update, tokens)
        })
        .collect::<Vec<_>>();

    let retained = retain_newest_section_updates(&updates, 32);

    assert!(approx_token_count(&retained) <= 32);
    assert!(retained.contains("update-199"));
    assert!(!retained.lines().any(|line| line == "update-0"));
}

#[test]
fn oversized_goal_section_retains_newest_text_within_budget() {
    let updates = vec![
        format!("ORIGINAL_CONSTRAINT {}", "original ".repeat(200)),
        "obsolete intermediate goal".to_string(),
        format!("LATEST_REVISION {}", "latest ".repeat(200)),
    ]
    .into_iter()
    .map(|update| {
        let tokens = approx_token_count(&update);
        (update, tokens)
    })
    .collect::<Vec<_>>();

    let retained = retain_latest_goal(&updates, 96);

    assert!(approx_token_count(&retained) <= 96);
    assert!(!retained.contains("ORIGINAL_CONSTRAINT"));
    // A partial paragraph could sever a qualification. Keep only the complete
    // fitting update; its position does not establish that it was superseded.
    assert!(!retained.contains("LATEST_REVISION"));
    assert_eq!(retained, "obsolete intermediate goal");
}

#[test]
fn summary_can_be_reused_when_only_new_user_input_follows_it() {
    let summary_text = format!("{SUMMARY_PREFIX}\nsettled state");
    let items = vec![
        compaction_summary_item(summary_text.clone()),
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
    assert_eq!(latest_summary_message(&items), Some(summary_text.as_str()));
    assert!(history_after_latest_summary_is_user_only(&items));
}

#[test]
fn incremental_goal_scope_reversal_survives_repeated_compaction() {
    let mut summary = bounded_task_state_summary(None,
        "## Goal\nPublish the project.\n\n## Evidence\nOriginal request: publish the project.");
    let current = format!("Investigate locally. {} DO NOT PUBLISH. {} Finish with findings only.",
        "scope detail ".repeat(60), "current detail ".repeat(60));
    assert!(approx_token_count(&current) > 250);
    summary = bounded_task_state_summary(Some(&summary), &format!("## Goal\n{current}"));
    for _ in 0..3 {
        summary = bounded_task_state_summary(Some(&summary), "## Current state\nStill investigating.");
        assert!(summary.contains(&current));
        assert!(summary.find("Publish the project.").unwrap() < summary.find("DO NOT PUBLISH.").unwrap());
        assert!(summary.contains("Original request: publish the project."));
        assert!(checkpoint_lines(&summary).any(|(_, index)| index == Some(0)));
        assert!(approx_token_count(&summary) <= COMPACT_TASK_STATE_MAX_TOKENS);
    }
}

#[test]
fn fitting_goal_updates_keep_middle_restrictions() {
    let summary = "## Goal\nInvestigate failures.\n\n## Goal\nDo not edit production files.\n\n## Goal\nInclude the reproduction.";
    assert_eq!(truncate_compaction_summary(summary, 500), summary);
    assert!(generated_summary_recovery_canonical(None, summary, summary).is_none());
}

#[test]
fn trimmed_generated_unresolved_work_remains_exactly_recoverable() {
    let source = format!("## Unresolved work\n{} FINAL-REQUIREMENT", "required action\n".repeat(10_000));
    let bounded = truncate_compaction_summary(&source, 200);
    assert!(!bounded.contains("FINAL-REQUIREMENT"));
    let canonical = generated_summary_recovery_canonical(None, &source, &bounded).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&canonical.bytes).unwrap();
    assert_eq!(value["items"][0], source);
}

#[test]
fn fitting_checkpoint_preserves_evidence_above_its_guidance_budget() {
    let summary = format!("## Goal\nDiagnose.\n\n## Evidence\n{} MIDDLE-FACT {}\n\n## Next action\nCheck the fact.",
        "observed detail ".repeat(100), "authenticated detail ".repeat(100));
    assert!(approx_token_count(&summary) > 500);
    assert!(approx_token_count(&summary) < COMPACT_TASK_STATE_MAX_TOKENS);
    assert_eq!(truncate_compaction_summary(&summary, COMPACT_TASK_STATE_MAX_TOKENS), summary);
}

#[test]
fn oversized_checkpoint_sections_borrow_spare_capacity() {
    let evidence = format!("{} MIDDLE-FACT {}", "observed detail ".repeat(100), "authenticated detail ".repeat(100));
    let summary = format!("{}\n## Goal\nDiagnose.\n\n## Current state\nInvestigating.\n\n## Completed work\nCaptured source.\n\n## Unresolved work\nCause unknown.\n\n## Evidence\n{evidence}\n\n## Next action\nCheck the fact.", "preamble ".repeat(2_000));
    assert!(approx_token_count(&summary) > COMPACT_TASK_STATE_MAX_TOKENS);
    let retained = truncate_compaction_summary(&summary, COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(retained.contains(evidence.trim()));
    assert!(approx_token_count(&retained) <= COMPACT_TASK_STATE_MAX_TOKENS);
    assert!(compaction_section_bodies(&retained).into_iter().all(|body| body));
}

#[test]
fn checkpoint_structure_ignores_fenced_quoted_and_indented_headings() {
    for (open, close) in [("```rust", "```"), ("~~~~text", "~~~~"), ("````", "````")] {
        let excerpt = format!("Source: example.rs\n{open}\n## Goal\nquoted goal\n```\n## Next action\nquoted action\n{close}");
        // A longer opening fence must not be closed by a shorter one. For the
        // three-backtick case use a tilde line inside instead.
        let excerpt = if open == "```rust" { excerpt.replace("\n```\n## Next action", "\n~~~\n## Next action") } else { excerpt };
        let summary = format!("## Goal\nCurrent task.\n\n## Evidence\n{excerpt}\n> ## Goal\n    ## Next action\n\n## Next action\nReal action.");
        let headings = checkpoint_lines(&summary).filter_map(|(_, section)| section).collect::<Vec<_>>();
        assert_eq!(headings, vec![0, 4, 5]);
        assert!(!has_compaction_section(&excerpt));
        let retained = bounded_task_state_summary(Some(&summary), "## Goal\nReplacement task.");
        assert!(retained.contains(&excerpt));
        assert!(retained.contains("## Next action\nReal action."));
        assert!(retained.contains("Current task."));
        assert_eq!(checkpoint_lines(&retained).filter_map(|(_, section)| section).collect::<Vec<_>>(), vec![0, 4, 5, 0]);
    }
}

#[test]
fn fresh_task_state_replaces_retained_plan_snapshots_without_dropping_mixed_text() {
    let message = |text: &str| {
        let mut item = ResponseItem::Message {
        id: None, role: "user".into(), content: vec![ContentItem::InputText { text: text.into() }],
        phase: None, internal_chat_message_metadata_passthrough: None,
        };
        crate::stable_context::mark_trusted_stable_context_item(&mut item);
        item
    };
    let mut mixed = message("<codex_task_state>old</codex_task_state>");
    if let ResponseItem::Message { content, .. } = &mut mixed {
        content.push(ContentItem::InputText { text: "Original user obligation".into() });
    }
    let current = message("<codex_task_state>current plan and unique lineage</codex_task_state>");
    let replaced = insert_compaction_initial_context(
        vec![mixed, message("<codex_internal_context source=\"compaction_plan\">duplicate plan</codex_internal_context>")],
        vec![current], &InitialContextInjection::DoNotInject,
    );
    let text = replaced.iter().filter_map(|item| match item {
        ResponseItem::Message { content, .. } => content_items_to_text(content), _ => None,
    }).collect::<Vec<_>>().join("\n");
    assert_eq!(text, "Original user obligation");
}

#[test]
fn token_backfire_compaction_summary_keeps_artifact_pin_sidecar_separate() {
    let summary_text = format!("{SUMMARY_PREFIX}\nsettled state");
    let pin_payload = serde_json::json!({
        "version": 1,
        "kind": "tool_history_artifact_pins",
        "artifacts": [{
            "version": 1,
            "kind": "tool_history_artifact_pin",
            "artifact_id": "artifact-1",
            "bytes": 96_000,
            "sha256": "abc123"
        }]
    })
    .to_string();

    let item =
        compaction_summary_item_with_artifact_pins(summary_text.clone(), Some(pin_payload.clone()));

    assert_eq!(compaction_summary_text(&item), Some(summary_text.as_str()));
    let ResponseItem::Message { content, .. } = item else {
        panic!("compaction summary must remain a message");
    };
    assert_eq!(
        content,
        vec![
            ContentItem::InputText { text: summary_text },
            ContentItem::InputText { text: pin_payload },
        ]
    );
}

#[test]
fn user_text_with_summary_prefix_does_not_spoof_checkpoint() {
    let prefixed_user_text = format!("{SUMMARY_PREFIX}\nuser-authored requirements");
    let items = vec![
        user_message(&prefixed_user_text),
        user_message("next request"),
    ];
    let (unresolved_history, _) = build_unresolved_user_history(&items);

    assert_eq!(latest_summary_message(&items), None);
    assert!(!history_after_latest_summary_is_user_only(&items));
    assert_eq!(unresolved_history, items);
    assert_eq!(
        collect_user_messages(&items),
        vec![
            compacted_user_message(&prefixed_user_text),
            compacted_user_message("next request"),
        ]
    );
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
    assert!(summary.ends_with(
        "[codex-local-compaction omitted user images: limits exceeded] Omitted image count: 2."
    ));
}

#[test]
fn unresolved_history_reports_exact_image_omissions() {
    for total in [0, MAX_RETAINED_USER_IMAGES, MAX_RETAINED_USER_IMAGES + 3] {
        let items = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: (0..total)
                .map(|index| ContentItem::InputImage {
                    image_url: format!("img-{index}"),
                    detail: None,
                })
                .collect(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        let (history, retained) = build_unresolved_user_history(&items);
        assert_eq!(retained, total.min(MAX_RETAINED_USER_IMAGES));
        let notices: Vec<_> = history
            .iter()
            .filter_map(|item| match item {
                ResponseItem::Message { content, .. } => content_items_to_text(content),
                _ => None,
            })
            .filter(|text| text.contains(COMPACT_IMAGE_OMISSION_MARKER))
            .collect();
        if total > MAX_RETAINED_USER_IMAGES {
            assert_eq!(notices, vec![
                "[codex-local-compaction omitted user images: limits exceeded] Omitted image count: 3.".to_string()
            ]);
        } else {
            assert!(
                notices.is_empty(),
                "no omission notice when every image fits"
            );
        }
    }
}

#[test]
fn build_token_limited_compacted_history_appends_summary_message() {
    let initial_context: Vec<ResponseItem> = Vec::new();
    let user_messages = vec![compacted_user_message("first user message")];
    let summary_text = format!("{SUMMARY_PREFIX}\nsummary text");

    let history = build_compacted_history(initial_context, &user_messages, &summary_text);
    assert!(
        !history.is_empty(),
        "expected compacted history to include summary"
    );

    let last = history.last().expect("history should have a summary entry");
    assert!(is_compaction_summary_item(last));
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

    assert!(should_use_remote_compact_task(&provider, None));
    assert!(
        !should_use_remote_compact_task(&provider, Some("custom compact prompt")),
        "a custom prompt must use local compaction so the prompt reaches the model"
    );
}

#[test]
fn incremental_guidance_does_not_repeat_or_override_custom_compact_prompt() {
    assert_eq!(
        incremental_summarization_prompt(Some("preserve the custom structure")),
        None,
        "the initial input already contains the custom prompt"
    );
    assert_eq!(
        incremental_summarization_prompt(None),
        Some(INCREMENTAL_SUMMARIZATION_PROMPT)
    );
    // The Markdown is runtime input, not documentation: exercise the same
    // prompt-plus-budget assembly used for an incremental compaction request.
    let previous = format!("{SUMMARY_PREFIX}\nverified state");
    let sections = compaction_rebase_sections(&previous);
    let assembled = format!("{}{}{}", incremental_summarization_prompt(None).unwrap(),
        compaction_rebase_prompt(&sections), compaction_update_budget_prompt(&previous, &sections));
    assert!(assembled.starts_with(include_str!("../../prompts/templates/compact/incremental_prompt.md")));
    assert!(assembled.contains("incremental update"));
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
    assert_regenerated_initial_context(&refreshed, expected);
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
    assert_regenerated_initial_context(&refreshed, expected);
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
    assert_regenerated_initial_context(&refreshed, expected);
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
    assert_regenerated_initial_context(&refreshed, initial_context);
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
        summary_message("summary text"),
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
    assert_regenerated_initial_context(&refreshed, initial_context);
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

    assert_regenerated_initial_context(&refreshed, initial_context);
}

#[test]
fn insert_initial_context_before_last_real_user_or_summary_keeps_summary_last() {
    let summary_item = summary_message("summary text");
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
        summary_item.clone(),
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
        summary_item,
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

#[test]
fn compaction_omission_metadata_has_a_fixed_budget() {
    let items = (0..5000)
        .map(|_| user_message("unresolved constraint "))
        .collect::<Vec<_>>();
    let (bounded, _, _, omitted, omitted_text) = build_bounded_unresolved_input_history(&items);
    assert!(omitted);
    let receipts = bounded
        .iter()
        .filter(|item| {
            serde_json::to_string(item)
                .unwrap()
                .contains(COMPACT_TEXT_OMISSION_MARKER)
        })
        .collect::<Vec<_>>();
    assert!(receipts.iter().any(|item| {
        serde_json::to_string(item)
            .unwrap()
            .contains("additional_omitted_messages")
    }));
    assert!(
        receipts
            .iter()
            .map(|item| response_item_text_tokens(item))
            .sum::<usize>()
            < 1200
    );
    let canonical = compaction_text_recovery_canonical(&items, omitted_text).unwrap();
    assert_eq!(
        canonical.value.as_ref().unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        5000
    );
}

#[tokio::test]
async fn local_compaction_retains_literal_omission_markers_and_reports_retained_images() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let literal = format!("Do not modify X. Explain the marker {COMPACT_TEXT_OMISSION_MARKER} in this image.");
    let mut encoded_image = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
        1,
        1,
        image::Rgba([10u8, 20, 30, 255]),
    ))
    .write_to(&mut encoded_image, image::ImageFormat::Png)
    .expect("encode retained image");
    let image = ContentItem::InputImage {
        image_url: codex_utils_image::data_url_from_bytes("image/png", encoded_image.get_ref()),
        detail: Some(DEFAULT_IMAGE_DETAIL),
    };
    let mut request = user_message(&literal);
    if let ResponseItem::Message { content, .. } = &mut request {
        content.push(image.clone());
    }
    session
        .record_conversation_items(&turn, &[summary_message("Modify X next."), request])
        .await
        .unwrap();
    // No text was omitted, so compaction must succeed even if recovery storage is unavailable.
    let artifact_path = turn.config.codex_home.join("tool-output");
    std::fs::create_dir_all(&turn.config.codex_home).unwrap();
    std::fs::write(&artifact_path, "blocked").unwrap();
    let session = Arc::new(session);
    let mut details = CompactionAnalyticsDetails::default();
    let summary = run_compact_task_inner_impl(
        Arc::clone(&session),
        Arc::new(turn),
        None,
        Some(&None),
        Vec::new(),
        InitialContextInjection::DoNotInject,
        CompactionTurnMetadata::new(
            CompactionTrigger::Manual,
            CompactionReason::UserRequested,
            CompactionImplementation::Responses,
            CompactionPhase::StandaloneTurn,
        ),
        &mut details,
        true,
        &CancellationToken::new(),
    )
    .await
    .expect("literal marker text must not require a recovery artifact");

    assert_eq!(summary, format!("{SUMMARY_PREFIX}\nModify X next."));
    assert_eq!(details.retained_image_count, Some(1));
    let history = session.clone_history().await;
    assert_eq!(history.raw_items().len(), 2);
    assert!(is_compaction_summary_item(&history.raw_items()[0]));
    let sampled = history.clone().prepare_for_sampling_prompt(
        &[codex_protocol::openai_models::InputModality::Text, codex_protocol::openai_models::InputModality::Image],
        crate::stable_context::StableContextTarget::Sampling,
    );
    assert!(is_compaction_summary_item(&sampled.items()[0]));
    let ResponseItem::Message { content, .. } = &sampled.items()[1] else {
        panic!("expected retained user request");
    };
    assert_eq!(
        content,
        &vec![ContentItem::InputText { text: literal }, image]
    );
    assert_eq!(std::fs::read_to_string(artifact_path).unwrap(), "blocked");
    assert_eq!(
        session.current_window_id().await,
        format!("{}:1", session.thread_id)
    );
}

#[tokio::test]
async fn compaction_recovery_failure_keeps_unresolved_text() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let text = "exact unresolved constraint ".repeat(COMPACT_USER_MESSAGE_MAX_TOKENS);
    session
        .record_conversation_items(&turn, &[user_message(&text)])
        .await
        .unwrap();
    let history = session.clone_history().await;
    let (_, _, _, omitted, _) = build_bounded_unresolved_input_history(history.raw_items());
    assert!(omitted);
    // Block the artifact directory with a file, forcing the real storage path to fail.
    std::fs::create_dir_all(&turn.config.codex_home).unwrap();
    std::fs::write(turn.config.codex_home.join("tool-output"), "blocked").unwrap();
    let session = Arc::new(session);
    let window_before = session.current_window_id().await;
    let result = run_compact_task_inner(
        Arc::clone(&session),
        Arc::new(turn),
        None,
        None,
        vec![UserInput::Text {
            text: SUMMARIZATION_PROMPT.to_string(),
            text_elements: Vec::new(),
        }],
        InitialContextInjection::DoNotInject,
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionPhase::StandaloneTurn,
        true,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(session.current_window_id().await, window_before);
    assert!(matches!(result, Err(CodexErr::Fatal(message)) if message ==
        "Compaction could not preserve exact unresolved text; original history was retained."));
    assert_eq!(
        session.clone_history().await.raw_items(),
        history.raw_items()
    );
}

#[tokio::test]
async fn compaction_recovery_survives_retention_resume_and_fork() {
    use crate::tool_history::{load_tool_history_state, persist_tool_history_state,
        remint_tool_history_state_for_fork, ToolHistoryLoadOutcome};
    use crate::tools::command_output_artifact::read_exact_tool_output_artifact;

    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let home = &turn.config.codex_home;
    let parent = session.thread_id().to_string();
    let canonical = compaction_text_recovery_for_items(vec![user_message(
        "Keep this exact earlier constraint: do not change Ω or its Unicode spelling.",
    )]);
    let sidecar = persist_compaction_recovery(&session, canonical.clone()).await.unwrap();
    let reference: serde_json::Value = serde_json::from_str(&sidecar).unwrap();
    let id = reference["artifact_id"].as_str().unwrap();
    let mut state = session.clone_history().await.tool_history_state();
    state.retain_for_history(&[compaction_summary_item_with_artifact_pins(
        format!("{SUMMARY_PREFIX}\nsummary"), Some(sidecar),
    )]);
    assert!(state.artifact_references().contains_key(id));
    persist_tool_history_state(home, &parent, &state).await.unwrap();

    // Force actual retention pressure with cheap legacy sparse artifacts, not
    // a mocked marker assertion. The recovery handle is the oldest artifact.
    let directory = home.join("tool-output").join(&parent);
    let pressure = (0..20).map(|_| {
        let path = directory.join(format!("{}.log", uuid::Uuid::new_v4()));
        std::fs::File::create(&path).unwrap().set_len(16 * 1024 * 1024).unwrap();
        path
    }).collect::<Vec<_>>();
    crate::tools::command_output_artifact::force_retention_reconciliation_for_test(
        &home.join("tool-output"),
    ).await;
    let trigger = create_canonical_output_artifact(home, &parent, &CanonicalToolResult::text("trigger retention")).await;
    assert!(trigger.complete);
    assert!(pressure.iter().any(|path| !path.exists()), "retention pressure must evict unprotected work");

    let ToolHistoryLoadOutcome::Loaded(resumed) = load_tool_history_state(home, &parent).await else {
        panic!("expected durable recovery ownership");
    };
    assert!(resumed.artifact_references().contains_key(id));
    let child = codex_protocol::ThreadId::new().to_string();
    let (forked, dropped) = remint_tool_history_state_for_fork(home, &parent, &child, resumed).await;
    assert_eq!(dropped, 0);
    persist_tool_history_state(home, &child, &forked).await.unwrap();
    let ToolHistoryLoadOutcome::Loaded(reopened) = load_tool_history_state(home, &child).await else {
        panic!("expected forked recovery ownership");
    };
    assert!(reopened.artifact_references().contains_key(id));
    for thread in [&parent, &child] {
        let exact = read_exact_tool_output_artifact(home, thread, id).await.unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&exact).unwrap(), canonical.value.clone().unwrap());
    }
}

#[tokio::test]
async fn valid_local_summary_is_not_published_when_replacement_preparation_fails() {
    use core_test_support::responses::{ev_assistant_message, ev_completed, mount_sse_once, sse, start_mock_server};

    let server = start_mock_server().await;
    let summary = COMPACTION_SECTIONS.iter()
        .map(|(heading, _)| format!("{heading}\nvalid tentative summary"))
        .collect::<Vec<_>>().join("\n\n");
    let request = mount_sse_once(&server, sse(vec![
        ev_assistant_message("tentative-summary", &summary), ev_completed("compaction-response"),
    ])).await;
    let home = tempfile::tempdir().unwrap();
    let (mut session, turn, _events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test"), Vec::new(), home.path(), |config| {
                config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                config.model_provider.supports_websockets = false;
            },
        ).await;
    crate::session::tests::attach_thread_persistence(Arc::get_mut(&mut session).unwrap()).await;
    session.record_conversation_items(&turn, &[user_message(
        "keep my exact request",
    )]).await.unwrap();
    let update = session.services.plan_store.update(codex_protocol::plan_tool::UpdatePlanArgs {
        explanation: None,
        plan: vec![codex_protocol::plan_tool::PlanItemArg {
            step: "exact large obligation ".repeat(COMPACT_TASK_STATE_MAX_TOKENS),
            status: codex_protocol::plan_tool::StepStatus::Pending,
        }],
    }).await;
    session.services.plan_store.update_tool(crate::plan_store::PlanToolArgs {
        expected_revision: Some(crate::plan_store::plan_revision_with_lineage(
            Some(&update.current), &update.lineage,
        )),
        plan: Some(vec![crate::plan_store::PlanStepArg {
            step: "Finish the original obligation".into(),
            status: codex_protocol::plan_tool::StepStatus::Pending,
            continues: vec![update.lineage.step_id(&update.current.plan[0].step)],
        }]),
        ..Default::default()
    }).await.unwrap();
    // The ordinary history append is available. Only replacement preparation
    // fails, after receiving a valid summarizer response, at unresolved-text recovery.
    std::fs::write(home.path().join("tool-output"), b"blocked recovery storage").unwrap();
    let before = session.clone_history().await.into_raw_items();
    let window = session.current_window_id().await;
    let result = run_compact_task_inner_impl(
        Arc::clone(&session), turn, None, Some(&None), Vec::new(),
        InitialContextInjection::DoNotInject,
        CompactionTurnMetadata::new(CompactionTrigger::Manual, CompactionReason::UserRequested,
            CompactionImplementation::Responses, CompactionPhase::StandaloneTurn),
        &mut CompactionAnalyticsDetails::default(), false, &CancellationToken::new(),
    ).await;
    assert!(matches!(result, Err(CodexErr::Fatal(message)) if message.contains("exact unresolved text")));
    assert_eq!(request.requests().len(), 1);
    assert_eq!(session.clone_history().await.raw_items(), before);
    assert_eq!(session.current_window_id().await, window);
    session.live_thread().unwrap().flush().await.unwrap();
    let persisted = session.live_thread().unwrap().load_history(false).await.unwrap();
    assert!(!persisted.items.iter().any(|item| match item {
        codex_protocol::protocol::RolloutItem::ResponseItem(item) =>
            serde_json::to_string(item).unwrap().contains("valid tentative summary"),
        codex_protocol::protocol::RolloutItem::Compacted(_) => true,
        _ => false,
    }));
}

#[tokio::test]
async fn local_compaction_keeps_consumed_resume_invalidation() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let notice = ResponseItem::Message {
        id: None, role: "developer".to_string(),
        content: vec![ContentItem::InputText { text: "<unified_exec_resume_invalidated>\nOld process handles are invalid; newly returned handles remain valid.\n</unified_exec_resume_invalidated>".to_string() }],
        phase: None, internal_chat_message_metadata_passthrough: None,
    };
    session
        .record_conversation_items(
            &turn,
            &[
                notice.clone(),
                summary_message("settled state"),
                user_message("continue"),
            ],
        )
        .await
        .unwrap();
    let session = Arc::new(session);
    run_compact_task_inner_impl(
        Arc::clone(&session),
        Arc::new(turn),
        None,
        Some(&None),
        Vec::new(),
        InitialContextInjection::DoNotInject,
        CompactionTurnMetadata::new(
            CompactionTrigger::Manual,
            CompactionReason::UserRequested,
            CompactionImplementation::Responses,
            CompactionPhase::StandaloneTurn,
        ),
        &mut CompactionAnalyticsDetails::default(),
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let history = session.clone_history().await;
    let retained = history
        .raw_items()
        .iter()
        .filter(|item| crate::session::is_unified_exec_resume_invalidation(item))
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 1);
    let mut retained = retained[0].clone();
    retained.set_id(None);
    if let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut retained
    {
        *internal_chat_message_metadata_passthrough = None;
    }
    assert_eq!(retained, notice);
}
#[test]
fn verified10_whole_qualifications_survive_every_paragraph_position() {
    let obligation = "Validation passed for the old revision only.\nDo NOT deploy; the integration failure remains unresolved.";
    for position in [0, 20, 40] {
        let mut paragraphs = (0..40).map(|index| format!("Observation {index}: {}", "diagnostic ".repeat(80))).collect::<Vec<_>>();
        paragraphs.insert(position, obligation.into());
        let generated = format!("{UNRESOLVED_WORK_HEADING}\n{}", paragraphs.join("\n\n"));
        let bounded = truncate_compaction_summary(&generated, 300);
        assert!(bounded.contains(obligation) || !bounded.contains("Validation passed"));
        assert!(bounded.contains(INCOMPLETE_CHECKPOINT_EXCERPT));
        assert!(generated_summary_recovery_canonical(None, &generated, &bounded).is_some());
        assert!(validate_generated_compaction_summary(None, &bounded).is_err());
    }
}

#[test]
fn verified10_first_request_accounts_for_aggregate_checkpoint_headroom() {
    let previous = COMPACTION_SECTIONS.iter().map(|(heading, budget)|
        format!("{heading}\n{}", "x ".repeat(budget * 3 / 2))).collect::<Vec<_>>().join("\n\n");
    assert!(approx_token_count(&previous) < COMPACT_TASK_STATE_MAX_TOKENS);
    let rebased = compaction_rebase_sections(&previous);
    assert_eq!(rebased, (0..COMPACTION_SECTIONS.len()).collect::<Vec<_>>());
    let budget = compaction_update_budget_prompt(&previous, &rebased);
    assert!(budget.contains("NOT to this update alone"));
    let suffix = COMPACTION_SECTIONS.iter().map(|(heading, budget)| {
        if *heading == UNRESOLVED_WORK_HEADING { format!("{heading}\n{}", "x ".repeat(budget * 3 / 2)) }
        else { format!("{heading}\nCurrent state; prior obligations unchanged.") }
    }).collect::<Vec<_>>().join("\n\n");
    let accepted = validated_rebased_compaction_summary(&previous, &suffix, &rebased).unwrap();
    assert!(approx_token_count(&accepted) <= COMPACT_TASK_STATE_MAX_TOKENS);
}
