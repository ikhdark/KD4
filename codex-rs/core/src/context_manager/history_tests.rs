use super::*;
use crate::context::UserInstructions;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSection;
use crate::stable_context::StableContextTarget;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::models::LocalShellStatus;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::default_input_modalities;
use codex_protocol::protocol::APPS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::PLUGINS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use image::ImageBuffer;
use image::ImageFormat;
use image::Luma;
use image::Rgba;
use pretty_assertions::assert_eq;
use regex_lite::Regex;

const EXEC_FORMAT_MAX_BYTES: usize = 10_000;
const EXEC_FORMAT_MAX_TOKENS: usize = 2_500;

fn assistant_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn inter_agent_assistant_msg(text: &str) -> ResponseItem {
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").unwrap(),
        Vec::new(),
        text.to_string(),
        /*trigger_turn*/ true,
    );
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: serde_json::to_string(&communication).unwrap(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn create_history_with_items(items: Vec<ResponseItem>) -> ContextManager {
    let mut h = ContextManager::new();
    // Use a generous but fixed token budget; tests only rely on truncation
    // behavior, not on a specific model's token limit.
    h.record_items(items.iter(), TruncationPolicy::Tokens(10_000));
    h
}

fn update_plan_pair(
    call_id: &str,
    arguments: &str,
    output: serde_json::Value,
) -> Vec<ResponseItem> {
    vec![
        ResponseItem::FunctionCall {
            id: None,
            name: "update_plan".to_string(),
            namespace: None,
            arguments: arguments.to_string(),
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_text(output.to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

#[test]
fn plan_history_projection_keeps_only_the_authoritative_current_plan() {
    let mut items = update_plan_pair(
        "plan-1",
        &"old plan detail ".repeat(500),
        serde_json::json!({
            "current_plan": {"explanation": null, "plan": [{"step": "old"}]},
            "validation_results": []
        }),
    );
    items.extend(update_plan_pair(
        "plan-2",
        &"current plan detail ".repeat(500),
        serde_json::json!({
            "current_plan": {"explanation": null, "plan": [{"step": "current"}]},
            "normalized_plan": {"explanation": null, "plan": [{"step": "current"}]},
            "validation_results": [{"outcome": "failure"}]
        }),
    ));
    let prepared = create_history_with_items(items).prepare_for_prompt(&default_input_modalities());
    let calls = prepared
        .items()
        .iter()
        .filter(
            |item| matches!(item, ResponseItem::FunctionCall { name, .. } if name == "update_plan"),
        )
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    let ResponseItem::FunctionCall {
        call_id, arguments, ..
    } = calls[0]
    else {
        unreachable!();
    };
    assert_eq!(call_id, "plan-2");
    assert!(arguments.contains("authoritative current plan"));

    let output = prepared
        .items()
        .iter()
        .find_map(|item| match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } if call_id == "plan-2" => output.body.to_text(),
            _ => None,
        })
        .expect("projected plan output");
    let output: serde_json::Value = serde_json::from_str(&output).expect("projected JSON");
    assert_eq!(output["current_plan"]["plan"][0]["step"], "current");
    assert_eq!(output["superseded_updates"], 1);
    assert!(output.get("normalized_plan").is_none());
    assert_eq!(output["validation_results"][0]["outcome"], "failure");
}

#[test]
fn plan_history_projection_fails_open_for_legacy_outputs() {
    let mut items = update_plan_pair(
        "plan-1",
        "{}",
        serde_json::json!({"message": "Plan updated"}),
    );
    items.extend(update_plan_pair(
        "plan-2",
        "{}",
        serde_json::json!({"message": "Plan updated"}),
    ));
    let prepared = create_history_with_items(items).prepare_for_prompt(&default_input_modalities());
    assert_eq!(
        prepared
            .items()
            .iter()
            .filter(|item| matches!(item, ResponseItem::FunctionCall { name, .. } if name == "update_plan"))
            .count(),
        2
    );
}

#[test]
fn prepared_history_fingerprint_ignores_rollout_turn_ids() {
    let item = |turn_id: &str| ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "unchanged model-visible history".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some(turn_id.to_string()),
        }),
    };
    let policy = PreparedHistoryPolicy {
        version: PREPARED_HISTORY_POLICY_VERSION,
        supports_images: true,
        stable_context_target: StableContextTarget::Sampling,
    };

    let first =
        prepared_history_fingerprint(&[item("turn-a")], &StableContextManifest::default(), policy)
            .expect("history should hash");
    let second =
        prepared_history_fingerprint(&[item("turn-b")], &StableContextManifest::default(), policy)
            .expect("history should hash");

    assert_eq!(first, second);
}

struct TestWorldStateSection;

impl WorldStateSection for TestWorldStateSection {
    const ID: &'static str = "test";
    type Snapshot = bool;

    fn snapshot(&self) -> Self::Snapshot {
        true
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user" && UserInstructions::matches_text(text)
    }

    fn render_diff(
        &self,
        previous: crate::context::world_state::PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn crate::context::ContextualUserFragment>> {
        let text = match previous {
            crate::context::world_state::PreviousSectionState::Known(true) => return None,
            crate::context::world_state::PreviousSectionState::Unknown => "unknown",
            crate::context::world_state::PreviousSectionState::Absent
            | crate::context::world_state::PreviousSectionState::Known(false) => "test",
        };
        Some(Box::new(UserInstructions {
            directory: None,
            text: text.to_string(),
        })
            as Box<dyn crate::context::ContextualUserFragment>)
    }
}

#[test]
fn world_state_baseline_deduplicates_until_history_is_replaced() {
    let world_state = || {
        let mut state = WorldState::default();
        state.add_section(TestWorldStateSection);
        state
    };
    let mut history = ContextManager::new();

    let (initial_fragments, initial_item) = history.update_world_state(&world_state());
    assert_eq!(1, initial_fragments.len());
    assert!(initial_item.is_some_and(|item| item.full));

    let (unchanged_fragments, unchanged_item) = history.update_world_state(&world_state());
    assert!(unchanged_fragments.is_empty());
    assert_eq!(unchanged_item, None);

    history.replace(Vec::new());

    let (replacement_fragments, replacement_item) = history.update_world_state(&world_state());
    assert_eq!(1, replacement_fragments.len());
    assert!(replacement_item.is_some_and(|item| item.full));
}

#[test]
fn world_state_reconciles_matching_legacy_history_once() {
    let item = crate::context::ContextualUserFragment::into(UserInstructions {
        directory: None,
        text: "legacy".to_string(),
    });
    let mut history = create_history_with_items(vec![item]);
    let mut world_state = WorldState::default();
    world_state.add_section(TestWorldStateSection);

    let (fragments, rollout_item) = history.update_world_state(&world_state);
    assert_eq!(
        vec!["# AGENTS.md instructions\n\n<INSTRUCTIONS>\nunknown\n</INSTRUCTIONS>"],
        fragments
            .into_iter()
            .map(|fragment| fragment.body())
            .collect::<Vec<_>>()
    );
    assert!(rollout_item.is_some_and(|item| item.full));

    let (fragments, rollout_item) = history.update_world_state(&world_state);
    assert!(fragments.is_empty());
    assert_eq!(rollout_item, None);
}

#[test]
fn world_state_baseline_retries_a_budget_rejected_section_on_the_next_update() {
    let world_state = || {
        let mut state = WorldState::default();
        for (index, (id, body)) in [("large_0", 'a'), ("large_1", 'b')].into_iter().enumerate() {
            state.add_extension_section(WorldStateSectionContribution::new(
                id,
                serde_json::json!({"value": index}),
                move |previous| match previous {
                    PreviousWorldStateSection::Absent => Some(RenderedWorldStateFragment::new(
                        "developer",
                        ("", ""),
                        body.to_string().repeat(30_000),
                    )),
                    PreviousWorldStateSection::Unknown | PreviousWorldStateSection::Known(_) => {
                        None
                    }
                },
            ));
        }
        state
    };
    let mut history = ContextManager::new();

    let (first_fragments, first_item) = history.update_world_state(&world_state());
    let (second_fragments, second_item) = history.update_world_state(&world_state());
    let (third_fragments, third_item) = history.update_world_state(&world_state());

    assert_eq!(
        first_fragments
            .into_iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        vec!["a".repeat(30_000)]
    );
    assert_eq!(
        first_item,
        Some(WorldStateItem::full(
            serde_json::json!({"large_0": {"value": 0}})
        ))
    );
    assert_eq!(
        second_fragments
            .into_iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        vec!["b".repeat(30_000)]
    );
    assert_eq!(
        second_item,
        Some(WorldStateItem::patch(
            serde_json::json!({"large_1": {"value": 1}})
        ))
    );
    assert!(third_fragments.is_empty());
    assert_eq!(third_item, None);
}

fn user_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn user_input_text_msg(text: &str) -> ResponseItem {
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

#[test]
fn finalization_projection_keeps_startup_checkpoint_and_only_requested_exact_artifact() {
    let startup = crate::context::ContextualUserFragment::into(UserInstructions {
        directory: None,
        text: "stable startup instructions".to_string(),
    });
    let mut items = vec![startup];
    for index in 0..200 {
        items.push(user_msg(&format!("historical user message {index}")));
        items.push(assistant_msg(&format!(
            "historical assistant message {index}"
        )));
    }
    for call_id in ["artifact-a", "artifact-b"] {
        items.push(ResponseItem::FunctionCall {
            id: None,
            name: crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_TOOL_NAME
                .to_string(),
            namespace: None,
            arguments: format!(r#"{{"artifact_id":"{call_id}"}}"#),
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        });
        items.push(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_text(format!("exact output {call_id}")),
            internal_chat_message_metadata_passthrough: None,
        });
    }
    let history = create_history_with_items(items);
    let prepared = history.prepare_for_finalization(
        &default_input_modalities(),
        CompletionCheckpointContext::new("checkpoint-required-material"),
        &BTreeSet::from(["artifact-a".to_string()]),
    );
    let rendered = serde_json::to_string(prepared.items()).expect("projected prompt serializes");
    assert!(rendered.contains("stable startup instructions"));
    assert!(rendered.contains("checkpoint-required-material"));
    assert!(rendered.contains("artifact-a"));
    assert!(rendered.contains("exact output artifact-a"));
    assert!(!rendered.contains("artifact-b"));
    assert!(!rendered.contains("historical user message"));
    assert!(!rendered.contains("historical assistant message"));
}

fn developer_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn developer_msg_with_fragments(texts: &[&str]) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: texts
            .iter()
            .map(|text| ContentItem::InputText {
                text: (*text).to_string(),
            })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn reference_context_item() -> TurnContextItem {
    TurnContextItem {
        turn_id: Some("reference-turn".to_string()),
        cwd: AbsolutePathBuf::try_from(
            std::env::current_dir()
                .expect("current directory")
                .join("reference-cwd"),
        )
        .expect("absolute reference cwd"),
        workspace_roots: None,
        current_date: Some("2026-03-23".to_string()),
        timezone: Some("America/Los_Angeles".to_string()),
        approval_policy: AskForApproval::OnRequest,
        approvals_reviewer: None,
        sandbox_policy: SandboxPolicy::new_read_only_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: "gpt-test".to_string(),
        comp_hash: None,
        personality: None,
        collaboration_mode: None,
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(false),
        effort: None,
        context_provenance: None,
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    }
}

fn custom_tool_call_output(call_id: &str, output: &str) -> ResponseItem {
    ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.to_string(),
        name: None,
        output: FunctionCallOutputPayload::from_text(output.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn custom_tool_call(call_id: &str) -> ResponseItem {
    ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: call_id.to_string(),
        name: "test_tool".to_string(),
        namespace: None,
        input: "{}".to_string(),
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

fn reasoning_msg(text: &str) -> ResponseItem {
    ResponseItem::Reasoning {
        id: None,
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: "summary".to_string(),
        }],
        content: Some(vec![ReasoningItemContent::ReasoningText {
            text: text.to_string(),
        }]),
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn reasoning_with_encrypted_content(len: usize) -> ResponseItem {
    ResponseItem::Reasoning {
        id: None,
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: "summary".to_string(),
        }],
        content: None,
        encrypted_content: Some("a".repeat(len)),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn reasoning_with_all_fields(text: &str, encrypted_content: &str) -> ResponseItem {
    ResponseItem::Reasoning {
        id: Some(ResponseItemId::from_server(
            "reasoning-all-fields".to_string(),
        )),
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: format!("summary: {text}"),
        }],
        content: Some(vec![ReasoningItemContent::ReasoningText {
            text: text.to_string(),
        }]),
        encrypted_content: Some(encrypted_content.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn truncate_exec_output(content: &str) -> String {
    truncate_text(content, TruncationPolicy::Tokens(EXEC_FORMAT_MAX_TOKENS))
}

fn approx_token_count_for_text(text: &str) -> i64 {
    i64::try_from(text.len().saturating_add(3) / 4).unwrap_or(i64::MAX)
}

#[test]
fn filters_non_api_messages() {
    let mut h = ContextManager::default();
    let policy = TruncationPolicy::Tokens(10_000);
    // System message is not API messages; Other is ignored.
    let system = ResponseItem::Message {
        id: None,
        role: "system".to_string(),
        content: vec![ContentItem::OutputText {
            text: "ignored".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let reasoning = reasoning_msg("thinking...");
    h.record_items([&system, &reasoning, &ResponseItem::Other], policy);

    // User and assistant should be retained.
    let u = user_msg("hi");
    let a = assistant_msg("hello");
    h.record_items([&u, &a], policy);

    let items = h.raw_items();
    assert_eq!(
        items,
        vec![
            ResponseItem::Reasoning {
                id: None,
                summary: vec![ReasoningItemReasoningSummary::SummaryText {
                    text: "summary".to_string(),
                }],
                content: Some(vec![ReasoningItemContent::ReasoningText {
                    text: "thinking...".to_string(),
                }]),
                encrypted_content: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "hi".to_string()
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "hello".to_string()
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
        ]
    );
}

#[test]
fn for_prompt_preserves_reasoning_when_no_instruction_boundary_exists() {
    let reasoning = reasoning_with_all_fields("private reasoning", "encrypted reasoning");
    let history = create_history_with_items(vec![reasoning.clone(), assistant_msg("done")]);

    assert_eq!(
        history.for_prompt(&default_input_modalities()),
        vec![reasoning, assistant_msg("done")]
    );
}

#[test]
fn for_prompt_evicts_entire_reasoning_item_before_latest_boundary_without_mutating_raw_history() {
    let resolved = reasoning_with_all_fields("private reasoning", "encrypted reasoning");
    let boundary = user_input_text_msg("next instruction");
    let history = create_history_with_items(vec![resolved.clone(), boundary.clone()]);
    let raw_before = history.raw_items().to_vec();

    assert_eq!(
        history.clone().for_prompt(&default_input_modalities()),
        vec![boundary]
    );
    assert_eq!(history.raw_items(), raw_before);
    assert_eq!(
        history.raw_items(),
        &[resolved, user_input_text_msg("next instruction")]
    );
}

#[test]
fn for_prompt_keeps_current_group_reasoning_through_tool_output_until_next_boundary() {
    let call = custom_tool_call("call-current");
    let reasoning = reasoning_msg("current reasoning");
    let output = custom_tool_call_output("call-current", "tool result");
    let history = create_history_with_items(vec![call.clone(), reasoning.clone(), output.clone()]);

    assert_eq!(
        history.clone().for_prompt(&default_input_modalities()),
        vec![call.clone(), reasoning, output.clone()]
    );

    let boundary = user_input_text_msg("new instruction");
    let mut after_boundary = history;
    after_boundary.record_items([&boundary], TruncationPolicy::Tokens(10_000));
    assert_eq!(
        after_boundary.for_prompt(&default_input_modalities()),
        vec![call, output, boundary]
    );
}

#[test]
fn for_prompt_uses_all_instruction_boundary_kinds() {
    for boundary in [
        user_input_text_msg("user instruction"),
        agent_message("agent instruction"),
        inter_agent_assistant_msg("structured instruction"),
    ] {
        let history = create_history_with_items(vec![reasoning_msg("resolved"), boundary.clone()]);
        assert_eq!(
            history.for_prompt(&default_input_modalities()),
            vec![boundary]
        );
    }
}

#[test]
fn for_prompt_does_not_treat_contextual_user_or_legacy_assistant_text_as_boundaries() {
    let contextual = crate::context::ContextualUserFragment::into(UserInstructions {
        directory: None,
        text: "context only".to_string(),
    });
    let legacy = assistant_msg(
        "author: /root\nrecipient: /root/worker\nother_recipients: []\nContent: continue",
    );
    let reasoning = reasoning_msg("still active");
    let history =
        create_history_with_items(vec![reasoning.clone(), contextual.clone(), legacy.clone()]);

    assert_eq!(
        history.for_prompt(&default_input_modalities()),
        vec![reasoning, contextual, legacy]
    );
}

#[test]
fn for_prompt_evicts_multiple_completed_groups_and_keeps_current_reasoning() {
    let first_boundary = user_input_text_msg("first");
    let second_boundary = user_input_text_msg("second");
    let current = reasoning_msg("current");
    let history = create_history_with_items(vec![
        reasoning_msg("before first"),
        first_boundary.clone(),
        reasoning_msg("before second"),
        second_boundary.clone(),
        current.clone(),
    ]);

    assert_eq!(
        history.for_prompt(&default_input_modalities()),
        vec![first_boundary, second_boundary, current]
    );
}

#[test]
fn for_prompt_projects_before_normalizing_retained_call_output_pairs() {
    let call = custom_tool_call("call-paired");
    let boundary = user_input_text_msg("next instruction");
    let output = custom_tool_call_output("call-paired", "tool result");
    let history = create_history_with_items(vec![
        reasoning_msg("resolved"),
        call.clone(),
        boundary.clone(),
        output.clone(),
    ]);

    assert_eq!(
        history.for_prompt(&default_input_modalities()),
        vec![call, boundary, output]
    );
}

#[test]
fn items_after_last_model_generated_tokens_include_user_and_tool_output() {
    let history = create_history_with_items(vec![
        assistant_msg("already counted by API"),
        user_msg("new user message"),
        custom_tool_call_output("call-tail", "new tool output"),
    ]);
    let expected_tokens = estimate_item_token_count(&user_msg("new user message")).saturating_add(
        estimate_item_token_count(&custom_tool_call_output("call-tail", "new tool output")),
    );

    assert_eq!(
        history
            .items_after_last_model_generated_item()
            .iter()
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add),
        expected_tokens
    );
}

#[test]
fn items_after_last_model_generated_tokens_are_zero_without_model_generated_items() {
    let history = create_history_with_items(vec![user_msg("no model output yet")]);

    assert_eq!(
        history
            .items_after_last_model_generated_item()
            .iter()
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add),
        0
    );
}

#[test]
fn inter_agent_assistant_messages_are_turn_boundaries() {
    let item = inter_agent_assistant_msg("continue");

    assert!(is_user_turn_boundary(&item));
}

#[test]
fn for_prompt_preserves_inter_agent_assistant_messages() {
    let item = inter_agent_assistant_msg("continue");
    let history = create_history_with_items(vec![item.clone()]);

    assert_eq!(history.raw_items(), std::slice::from_ref(&item));
    assert_eq!(history.for_prompt(&default_input_modalities()), vec![item]);
}

#[test]
fn drop_last_n_user_turns_treats_inter_agent_assistant_messages_as_instruction_turns() {
    let first_turn = user_input_text_msg("first");
    let first_reply = assistant_msg("done");
    let inter_agent_turn = inter_agent_assistant_msg("continue");
    let inter_agent_reply = assistant_msg("worker reply");
    let mut history = create_history_with_items(vec![
        first_turn.clone(),
        first_reply.clone(),
        inter_agent_turn,
        inter_agent_reply,
    ]);

    history.drop_last_n_user_turns(/*num_turns*/ 1);

    assert_eq!(history.raw_items(), &vec![first_turn, first_reply]);
}

#[test]
fn legacy_inter_agent_assistant_messages_are_not_turn_boundaries() {
    let item = assistant_msg(
        "author: /root\nrecipient: /root/worker\nother_recipients: []\nContent: continue",
    );

    assert!(!is_user_turn_boundary(&item));
}

#[test]
fn total_token_usage_keeps_server_snapshot_plus_same_group_tool_tail_in_both_modes() {
    let mut history = create_history_with_items(vec![assistant_msg("already counted by API")]);
    history.update_token_info(
        &TokenUsage {
            total_tokens: 100,
            ..Default::default()
        },
        /*model_context_window*/ None,
    );
    let added_tool_output = custom_tool_call_output("tool-tail", "new tool output");
    history.record_items([&added_tool_output], TruncationPolicy::Tokens(10_000));
    let base_instructions = BaseInstructions {
        text: "base instructions".to_string(),
    };

    for server_reasoning_included in [false, true] {
        assert_eq!(
            history.get_total_token_usage(server_reasoning_included, &base_instructions),
            100 + estimate_item_token_count(&added_tool_output)
        );
    }
}

#[test]
fn total_token_usage_recomputes_projected_history_when_local_tail_contains_boundary() {
    let resolved_reasoning = reasoning_with_encrypted_content(/*len*/ 2_000);
    let counted_response = assistant_msg("already counted by API");
    let boundary = user_input_text_msg("new instruction");
    let mut history =
        create_history_with_items(vec![resolved_reasoning, counted_response, boundary]);
    history.update_token_info(
        &TokenUsage {
            total_tokens: 50_000,
            ..Default::default()
        },
        /*model_context_window*/ None,
    );
    let base_instructions = BaseInstructions {
        text: "base instructions".to_string(),
    };
    let expected = ContextManager::estimate_items_token_count_with_base_instructions(
        history.raw_items(),
        &base_instructions,
    )
    .unwrap();

    for server_reasoning_included in [false, true] {
        assert_eq!(
            history.get_total_token_usage(server_reasoning_included, &base_instructions),
            expected
        );
    }
    assert!(expected < 50_000);
}

#[test]
fn total_token_usage_recomputes_initial_projected_history_before_any_model_response() {
    let boundary = user_input_text_msg("initial instruction");
    let history = create_history_with_items(vec![boundary]);
    let base_instructions = BaseInstructions {
        text: "base instructions".to_string(),
    };
    let expected = history
        .estimate_token_count_with_base_instructions(&base_instructions)
        .unwrap();

    for server_reasoning_included in [false, true] {
        assert_eq!(
            history.get_total_token_usage(server_reasoning_included, &base_instructions),
            expected
        );
    }
}

#[test]
fn total_token_usage_refreshes_from_server_after_next_model_response() {
    let boundary = user_input_text_msg("new instruction");
    let mut history = create_history_with_items(vec![
        reasoning_with_encrypted_content(/*len*/ 2_000),
        assistant_msg("old response"),
        boundary,
    ]);
    history.update_token_info(
        &TokenUsage {
            total_tokens: 50_000,
            ..Default::default()
        },
        /*model_context_window*/ None,
    );
    let fresh_response = assistant_msg("fresh response");
    history.record_items([&fresh_response], TruncationPolicy::Tokens(10_000));
    history.update_token_info(
        &TokenUsage {
            total_tokens: 222,
            ..Default::default()
        },
        /*model_context_window*/ None,
    );
    let base_instructions = BaseInstructions {
        text: "base instructions".to_string(),
    };

    for server_reasoning_included in [false, true] {
        assert_eq!(
            history.get_total_token_usage(server_reasoning_included, &base_instructions),
            222
        );
    }
}

#[test]
fn static_token_estimator_excludes_reasoning_before_latest_boundary() {
    let base_instructions = BaseInstructions {
        text: "base instructions".to_string(),
    };
    let visible = vec![assistant_msg("response"), user_input_text_msg("next")];
    let mut raw = vec![reasoning_with_encrypted_content(/*len*/ 4_000)];
    raw.extend(visible.clone());

    assert_eq!(
        ContextManager::estimate_items_token_count_with_base_instructions(&raw, &base_instructions),
        ContextManager::estimate_items_token_count_with_base_instructions(
            &visible,
            &base_instructions
        )
    );
}

#[test]
fn for_prompt_strips_images_when_model_does_not_support_images() {
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "look at this".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "https://example.com/img.png".to_string(),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
                ContentItem::InputText {
                    text: "caption".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "view_image".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "image result".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "https://example.com/result.png".to_string(),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "tool-1".to_string(),
            name: "js_repl".to_string(),
            namespace: None,
            input: "view_image".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "tool-1".to_string(),
            name: None,
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "js repl result".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "https://example.com/js-repl-result.png".to_string(),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let history = create_history_with_items(items);
    let text_only_modalities = vec![InputModality::Text];
    let stripped = history.for_prompt(&text_only_modalities);

    let expected = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "look at this".to_string(),
                },
                ContentItem::InputText {
                    text: "image content omitted because you do not support image input"
                        .to_string(),
                },
                ContentItem::InputText {
                    text: "caption".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "view_image".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "image result".to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "image content omitted because you do not support image input"
                        .to_string(),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "tool-1".to_string(),
            name: "js_repl".to_string(),
            namespace: None,
            input: "view_image".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "tool-1".to_string(),
            name: None,
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "js repl result".to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "image content omitted because you do not support image input"
                        .to_string(),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(stripped, expected);

    // With image support, images are preserved
    let modalities = default_input_modalities();
    let with_images = create_history_with_items(vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "look".to_string(),
            },
            ContentItem::InputImage {
                image_url: "https://example.com/img.png".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }]);
    let preserved = with_images.for_prompt(&modalities);
    assert_eq!(preserved.len(), 1);
    if let ResponseItem::Message { content, .. } = &preserved[0] {
        assert_eq!(content.len(), 2);
        assert!(matches!(content[1], ContentItem::InputImage { .. }));
    } else {
        panic!("expected Message");
    }
}

#[test]
fn for_prompt_preserves_image_generation_calls_when_images_are_supported() {
    let history = create_history_with_items(vec![
        ResponseItem::ImageGenerationCall {
            id: Some(ResponseItemId::with_suffix("ig", "123")),
            status: "generating".to_string(),
            revised_prompt: Some("lobster".to_string()),
            result: "Zm9v".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hi".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ]);

    assert_eq!(
        history.for_prompt(&default_input_modalities()),
        vec![
            ResponseItem::ImageGenerationCall {
                id: Some(ResponseItemId::with_suffix("ig", "123")),
                status: "generating".to_string(),
                revised_prompt: Some("lobster".to_string()),
                result: "Zm9v".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hi".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
        ]
    );
}

#[test]
fn for_prompt_clears_image_generation_result_when_images_are_unsupported() {
    let history = create_history_with_items(vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "generate a lobster".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ImageGenerationCall {
            id: Some(ResponseItemId::with_suffix("ig", "123")),
            status: "completed".to_string(),
            revised_prompt: Some("lobster".to_string()),
            result: "Zm9v".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ]);

    assert_eq!(
        history.for_prompt(&[InputModality::Text]),
        vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "generate a lobster".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ImageGenerationCall {
                id: Some(ResponseItemId::with_suffix("ig", "123")),
                status: "completed".to_string(),
                revised_prompt: Some("lobster".to_string()),
                result: String::new(),
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
}

#[test]
fn estimate_token_count_with_base_instructions_uses_provided_text() {
    let history = create_history_with_items(vec![assistant_msg("hello from history")]);
    let short_base = BaseInstructions {
        text: "short".to_string(),
    };
    let long_base = BaseInstructions {
        text: "x".repeat(1_000),
    };

    let short_estimate = history
        .estimate_token_count_with_base_instructions(&short_base)
        .expect("token estimate");
    let long_estimate = history
        .estimate_token_count_with_base_instructions(&long_base)
        .expect("token estimate");

    let expected_delta = approx_token_count_for_text(&long_base.text)
        - approx_token_count_for_text(&short_base.text);
    assert_eq!(long_estimate - short_estimate, expected_delta);
}

#[test]
fn replace_last_turn_images_replaces_tool_output_images() {
    let items = vec![
        user_input_text_msg("hi"),
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,AAA".to_string(),
                        detail: Some(DEFAULT_IMAGE_DETAIL),
                    },
                ]),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let mut history = create_history_with_items(items);

    assert!(history.replace_last_turn_images("Invalid image"));

    assert_eq!(
        history.raw_items(),
        vec![
            user_input_text_msg("hi"),
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-1".to_string(),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::ContentItems(vec![
                        FunctionCallOutputContentItem::InputText {
                            text: "Invalid image".to_string(),
                        },
                    ]),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
}

#[test]
fn replace_last_turn_images_finds_image_before_later_text_output() {
    let later_text_output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-2".to_string(),
        output: FunctionCallOutputPayload::from_text("later output".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut history = create_history_with_items(vec![
        user_input_text_msg("hi"),
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAA".to_string(),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
        later_text_output.clone(),
    ]);

    assert!(history.replace_last_turn_images("Invalid image"));
    assert_eq!(
        history.raw_items(),
        vec![
            user_input_text_msg("hi"),
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-1".to_string(),
                output: FunctionCallOutputPayload::from_content_items(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "Invalid image".to_string(),
                    },
                ]),
                internal_chat_message_metadata_passthrough: None,
            },
            later_text_output,
        ]
    );
}

#[test]
fn replace_last_turn_images_replaces_images_in_every_output_once() {
    let mut history = create_history_with_items(vec![
        user_input_text_msg("hi"),
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAA".to_string(),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "call-2".to_string(),
            name: None,
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,BBB".to_string(),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
            ]),
            internal_chat_message_metadata_passthrough: None,
        },
    ]);
    let previous_history_version = history.history_version();

    assert!(history.replace_last_turn_images("Invalid image"));
    assert_eq!(
        history.raw_items(),
        vec![
            user_input_text_msg("hi"),
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-1".to_string(),
                output: FunctionCallOutputPayload::from_content_items(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "Invalid image".to_string(),
                    },
                ]),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "call-2".to_string(),
                name: None,
                output: FunctionCallOutputPayload::from_content_items(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "Invalid image".to_string(),
                    },
                ]),
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
    assert_eq!(history.history_version(), previous_history_version + 1);
}

#[test]
fn replace_last_turn_images_does_not_touch_user_images() {
    let items = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputImage {
            image_url: "data:image/png;base64,AAA".to_string(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut history = create_history_with_items(items.clone());

    assert!(!history.replace_last_turn_images("Invalid image"));
    assert_eq!(history.raw_items(), items);
}

#[test]
fn drop_last_n_user_turns_preserves_prefix() {
    let items = vec![
        assistant_msg("session prefix item"),
        user_msg("u1"),
        assistant_msg("a1"),
        user_msg("u2"),
        assistant_msg("a2"),
    ];

    let modalities = default_input_modalities();
    let mut history = create_history_with_items(items);
    history.drop_last_n_user_turns(/*num_turns*/ 1);
    assert_eq!(
        history.for_prompt(&modalities),
        vec![
            assistant_msg("session prefix item"),
            user_msg("u1"),
            assistant_msg("a1"),
        ]
    );

    let mut history = create_history_with_items(vec![
        assistant_msg("session prefix item"),
        user_msg("u1"),
        assistant_msg("a1"),
        user_msg("u2"),
        assistant_msg("a2"),
    ]);
    history.drop_last_n_user_turns(/*num_turns*/ 99);
    assert_eq!(
        history.for_prompt(&modalities),
        vec![assistant_msg("session prefix item")]
    );
}

#[test]
fn drop_last_n_user_turns_without_user_turns_is_a_true_noop() {
    let items = vec![assistant_msg("session prefix item")];
    let mut history = create_history_with_items(items.clone());
    let previous_history_version = history.history_version();

    history.drop_last_n_user_turns(/*num_turns*/ 1);

    assert_eq!(history.raw_items(), items);
    assert_eq!(history.history_version(), previous_history_version);
}

#[test]
fn drop_last_n_user_turns_ignores_session_prefix_user_messages() {
    let items = vec![
        user_input_text_msg("<environment_context>ctx</environment_context>"),
        user_input_text_msg(
            "# AGENTS.md instructions for test_directory\n\n<INSTRUCTIONS>\ntest_text\n</INSTRUCTIONS>",
        ),
        user_input_text_msg(
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
        ),
        user_input_text_msg("<user_shell_command>echo 42</user_shell_command>"),
        user_input_text_msg(
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
        ),
        user_input_text_msg("turn 1 user"),
        assistant_msg("turn 1 assistant"),
        user_input_text_msg("turn 2 user"),
        assistant_msg("turn 2 assistant"),
    ];

    let modalities = default_input_modalities();
    let mut history = create_history_with_items(items);
    history.drop_last_n_user_turns(/*num_turns*/ 1);

    let expected_prefix_and_first_turn = vec![
        user_input_text_msg("<environment_context>ctx</environment_context>"),
        user_input_text_msg(
            "# AGENTS.md instructions for test_directory\n\n<INSTRUCTIONS>\ntest_text\n</INSTRUCTIONS>",
        ),
        user_input_text_msg(
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
        ),
        user_input_text_msg("<user_shell_command>echo 42</user_shell_command>"),
        user_input_text_msg(
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
        ),
        user_input_text_msg("turn 1 user"),
        assistant_msg("turn 1 assistant"),
    ];

    assert_eq!(
        history.for_prompt(&modalities),
        expected_prefix_and_first_turn
    );

    let expected_prefix_only = vec![
        user_input_text_msg("<environment_context>ctx</environment_context>"),
        user_input_text_msg(
            "# AGENTS.md instructions for test_directory\n\n<INSTRUCTIONS>\ntest_text\n</INSTRUCTIONS>",
        ),
        user_input_text_msg(
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
        ),
        user_input_text_msg("<user_shell_command>echo 42</user_shell_command>"),
        user_input_text_msg(
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
        ),
    ];

    let mut history = create_history_with_items(vec![
        user_input_text_msg("<environment_context>ctx</environment_context>"),
        user_input_text_msg(
            "# AGENTS.md instructions for test_directory\n\n<INSTRUCTIONS>\ntest_text\n</INSTRUCTIONS>",
        ),
        user_input_text_msg(
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
        ),
        user_input_text_msg("<user_shell_command>echo 42</user_shell_command>"),
        user_input_text_msg(
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
        ),
        user_input_text_msg("turn 1 user"),
        assistant_msg("turn 1 assistant"),
        user_input_text_msg("turn 2 user"),
        assistant_msg("turn 2 assistant"),
    ]);
    history.drop_last_n_user_turns(/*num_turns*/ 2);
    assert_eq!(history.for_prompt(&modalities), expected_prefix_only);

    let mut history = create_history_with_items(vec![
        user_input_text_msg("<environment_context>ctx</environment_context>"),
        user_input_text_msg(
            "# AGENTS.md instructions for test_directory\n\n<INSTRUCTIONS>\ntest_text\n</INSTRUCTIONS>",
        ),
        user_input_text_msg(
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
        ),
        user_input_text_msg("<user_shell_command>echo 42</user_shell_command>"),
        user_input_text_msg(
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
        ),
        user_input_text_msg("turn 1 user"),
        assistant_msg("turn 1 assistant"),
        user_input_text_msg("turn 2 user"),
        assistant_msg("turn 2 assistant"),
    ]);
    history.drop_last_n_user_turns(/*num_turns*/ 3);
    assert_eq!(history.for_prompt(&modalities), expected_prefix_only);
}

#[test]
fn drop_last_n_user_turns_trims_context_updates_above_rolled_back_turn() {
    let items = vec![
        assistant_msg("session prefix item"),
        user_input_text_msg("turn 1 user"),
        assistant_msg("turn 1 assistant"),
        developer_msg("Generated images are saved to /tmp as /tmp/image-1.png by default."),
        developer_msg(&format!(
            "{APPS_INSTRUCTIONS_OPEN_TAG}\nROLLED_BACK_APPS_INSTRUCTIONS"
        )),
        developer_msg(&format!(
            "{PLUGINS_INSTRUCTIONS_OPEN_TAG}\nROLLED_BACK_PLUGIN_INSTRUCTIONS"
        )),
        developer_msg("<collaboration_mode>ROLLED_BACK_DEV_INSTRUCTIONS</collaboration_mode>"),
        developer_msg("<multi_agent_mode>ROLLED_BACK_MULTI_AGENT_MODE</multi_agent_mode>"),
        user_input_text_msg(
            "<environment_context><cwd>PRETURN_CONTEXT_DIFF_CWD</cwd></environment_context>",
        ),
        user_input_text_msg("turn 2 user"),
        assistant_msg("turn 2 assistant"),
    ];

    let modalities = default_input_modalities();
    let mut history = create_history_with_items(items);
    let reference_context_item = reference_context_item();
    history.set_reference_context_item(Some(reference_context_item.clone()));
    history.drop_last_n_user_turns(/*num_turns*/ 1);

    assert_eq!(
        history.clone().for_prompt(&modalities),
        vec![
            assistant_msg("session prefix item"),
            user_input_text_msg("turn 1 user"),
            assistant_msg("turn 1 assistant"),
            developer_msg("Generated images are saved to /tmp as /tmp/image-1.png by default."),
        ]
    );
    assert_eq!(
        serde_json::to_value(history.reference_context_item())
            .expect("serialize retained reference context item"),
        serde_json::to_value(Some(reference_context_item))
            .expect("serialize expected reference context item")
    );
}

#[test]
fn drop_last_n_user_turns_clears_reference_context_for_mixed_developer_context_bundles() {
    let items = vec![
        user_input_text_msg("turn 1 user"),
        assistant_msg("turn 1 assistant"),
        developer_msg_with_fragments(&[
            "<permissions instructions>contextual permissions</permissions instructions>",
            "persistent plugin instructions",
        ]),
        user_input_text_msg(
            "<environment_context><cwd>PRETURN_CONTEXT_DIFF_CWD</cwd></environment_context>",
        ),
        user_input_text_msg("turn 2 user"),
        assistant_msg("turn 2 assistant"),
    ];

    let modalities = default_input_modalities();
    let mut history = create_history_with_items(items);
    history.set_reference_context_item(Some(reference_context_item()));
    history.drop_last_n_user_turns(/*num_turns*/ 1);

    assert_eq!(
        history.clone().for_prompt(&modalities),
        vec![
            user_input_text_msg("turn 1 user"),
            assistant_msg("turn 1 assistant"),
        ]
    );
    assert!(history.reference_context_item().is_none());
}

#[test]
fn normalization_retains_local_shell_outputs() {
    let items = vec![
        ResponseItem::LocalShellCall {
            id: None,
            call_id: Some("shell-1".to_string()),
            status: LocalShellStatus::Completed,
            action: LocalShellAction::Exec(LocalShellExecAction {
                command: vec!["echo".to_string(), "hi".to_string()],
                timeout_ms: None,
                working_directory: None,
                env: None,
                user: None,
            }),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "shell-1".to_string(),
            output: FunctionCallOutputPayload::from_text("Total output lines: 1\n\nok".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let modalities = default_input_modalities();
    let history = create_history_with_items(items.clone());
    let normalized = history.for_prompt(&modalities);
    assert_eq!(normalized, items);
}

#[test]
fn record_items_truncates_function_call_output_content() {
    let mut history = ContextManager::new();
    // Any reasonably small token budget works; the test only cares that
    // truncation happens and the marker is present.
    let policy = TruncationPolicy::Tokens(1_000);
    let long_line = "a very long line to trigger truncation\n";
    let long_output = long_line.repeat(2_500);
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-100".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(long_output.clone()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("turn-1".to_string()),
        }),
    };

    history.record_items([&item], policy);

    assert_eq!(history.items.len(), 1);
    match &history.items[0] {
        ResponseItem::FunctionCallOutput { output, .. } => {
            let content = output.text_content().unwrap_or_default();
            assert_ne!(content, long_output);
            assert!(
                content.contains("tokens truncated"),
                "expected token-based truncation marker, got {content}"
            );
            assert!(
                content.contains("tokens truncated"),
                "expected truncation marker, got {content}"
            );
            assert!(approx_token_count(content) <= policy.token_budget());
        }
        other => panic!("unexpected history item: {other:?}"),
    }
    assert_eq!(history.items[0].turn_id(), Some("turn-1"));
}

#[test]
fn record_items_truncates_custom_tool_call_output_content() {
    let mut history = ContextManager::new();
    let policy = TruncationPolicy::Tokens(1_000);
    let line = "custom output that is very long\n";
    let long_output = line.repeat(2_500);
    let item = ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "tool-200".to_string(),
        name: None,
        output: FunctionCallOutputPayload::from_text(long_output.clone()),
        internal_chat_message_metadata_passthrough: None,
    };

    history.record_items([&item], policy);

    assert_eq!(history.items.len(), 1);
    match &history.items[0] {
        ResponseItem::CustomToolCallOutput { output, .. } => {
            let output = output.text_content().unwrap_or_default();
            assert_ne!(output, long_output);
            assert!(
                output.contains("tokens truncated"),
                "expected token-based truncation marker, got {output}"
            );
            assert!(
                output.contains("tokens truncated") || output.contains("bytes truncated"),
                "expected truncation marker, got {output}"
            );
            assert!(approx_token_count(output) <= policy.token_budget());
        }
        other => panic!("unexpected history item: {other:?}"),
    }
}

#[test]
fn record_items_respects_custom_token_limit() {
    let mut history = ContextManager::new();
    let policy = TruncationPolicy::Tokens(10);
    let long_output = "tokenized content repeated many times ".repeat(200);
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-custom-limit".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(long_output),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };

    history.record_items([&item], policy);

    let stored = match &history.items[0] {
        ResponseItem::FunctionCallOutput { output, .. } => output,
        other => panic!("unexpected history item: {other:?}"),
    };
    assert!(
        stored
            .text_content()
            .is_some_and(|content| content.contains("tokens truncated"))
    );
}

fn assert_truncated_message_matches(message: &str, line: &str, expected_removed: usize) {
    let pattern = truncated_message_pattern(line);
    let regex = Regex::new(&pattern).unwrap_or_else(|err| {
        panic!("failed to compile regex {pattern}: {err}");
    });
    let captures = regex
        .captures(message)
        .unwrap_or_else(|| panic!("message failed to match pattern {pattern}: {message}"));
    let body = captures
        .name("body")
        .expect("missing body capture")
        .as_str();
    assert!(
        body.len() <= EXEC_FORMAT_MAX_BYTES,
        "body exceeds byte limit: {} bytes",
        body.len()
    );
    let removed: usize = captures
        .name("removed")
        .expect("missing removed capture")
        .as_str()
        .parse()
        .unwrap_or_else(|err| panic!("invalid removed tokens: {err}"));
    assert_eq!(removed, expected_removed, "mismatched removed token count");
}

fn truncated_message_pattern(line: &str) -> String {
    let escaped_line = regex_lite::escape(line);
    format!(r"(?s)^(?P<body>{escaped_line}.*?)(?:\r?)?…(?P<removed>\d+) tokens truncated…(?:.*)?$")
}

#[test]
fn format_exec_output_truncates_large_error() {
    let line = "very long execution error line that should trigger truncation\n";
    let large_error = line.repeat(2_500); // way beyond both byte and line limits

    let truncated = truncate_exec_output(&large_error);

    assert_truncated_message_matches(&truncated, line, /*expected_removed*/ 36250);
    assert_ne!(truncated, large_error);
}

#[test]
fn format_exec_output_marks_byte_truncation_without_omitted_lines() {
    let long_line = "a".repeat(EXEC_FORMAT_MAX_BYTES + 10000);
    let truncated = truncate_exec_output(&long_line);
    assert_ne!(truncated, long_line);
    assert_truncated_message_matches(&truncated, "a", /*expected_removed*/ 2500);
    assert!(
        !truncated.contains("omitted"),
        "line omission marker should not appear when no lines were dropped: {truncated}"
    );
}

#[test]
fn format_exec_output_returns_original_when_within_limits() {
    let content = "example output\n".repeat(10);
    assert_eq!(truncate_exec_output(&content), content);
}

#[test]
fn format_exec_output_reports_omitted_lines_and_keeps_head_and_tail() {
    let total_lines = 2_000;
    let filler = "x".repeat(64);
    let content: String = (0..total_lines)
        .map(|idx| format!("line-{idx}-{filler}\n"))
        .collect();

    let truncated = truncate_exec_output(&content);
    assert_truncated_message_matches(&truncated, "line-0-", /*expected_removed*/ 34_723);
    assert!(
        truncated.contains("line-0-"),
        "expected head line to remain: {truncated}"
    );

    let last_line = format!("line-{}-", total_lines - 1);
    assert!(
        truncated.contains(&last_line),
        "expected tail line to remain: {truncated}"
    );
}

#[test]
fn format_exec_output_prefers_line_marker_when_both_limits_exceeded() {
    let total_lines = 300;
    let long_line = "x".repeat(256);
    let content: String = (0..total_lines)
        .map(|idx| format!("line-{idx}-{long_line}\n"))
        .collect();

    let truncated = truncate_exec_output(&content);

    assert_truncated_message_matches(&truncated, "line-0-", /*expected_removed*/ 17_423);
}

#[cfg(not(debug_assertions))]
#[test]
fn normalize_adds_missing_output_for_function_call() {
    let items = vec![ResponseItem::FunctionCall {
        id: None,
        name: "do_it".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "call-x".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(
        h.raw_items(),
        vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "do_it".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "call-x".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-x".to_string(),
                output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
}

#[cfg(not(debug_assertions))]
#[test]
fn normalize_adds_missing_output_for_custom_tool_call() {
    let items = vec![ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: "tool-x".to_string(),
        name: "custom".to_string(),
        namespace: None,
        input: "{}".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(
        h.raw_items(),
        vec![
            ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "tool-x".to_string(),
                name: "custom".to_string(),
                namespace: None,
                input: "{}".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "tool-x".to_string(),
                name: None,
                output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
}

#[cfg(not(debug_assertions))]
#[test]
fn normalize_adds_missing_output_for_local_shell_call_with_id() {
    let items = vec![ResponseItem::LocalShellCall {
        id: None,
        call_id: Some("shell-1".to_string()),
        status: LocalShellStatus::Completed,
        action: LocalShellAction::Exec(LocalShellExecAction {
            command: vec!["echo".to_string(), "hi".to_string()],
            timeout_ms: None,
            working_directory: None,
            env: None,
            user: None,
        }),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(
        h.raw_items(),
        vec![
            ResponseItem::LocalShellCall {
                id: None,
                call_id: Some("shell-1".to_string()),
                status: LocalShellStatus::Completed,
                action: LocalShellAction::Exec(LocalShellExecAction {
                    command: vec!["echo".to_string(), "hi".to_string()],
                    timeout_ms: None,
                    working_directory: None,
                    env: None,
                    user: None,
                }),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "shell-1".to_string(),
                output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
}

#[cfg(not(debug_assertions))]
#[test]
fn normalize_removes_orphan_function_call_output() {
    let items = vec![ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "orphan-1".to_string(),
        output: FunctionCallOutputPayload::from_text("ok".to_string()),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(h.raw_items(), vec![]);
}

#[cfg(not(debug_assertions))]
#[test]
fn normalize_removes_orphan_custom_tool_call_output() {
    let items = vec![ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "orphan-2".to_string(),
        name: None,
        output: FunctionCallOutputPayload::from_text("ok".to_string()),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(h.raw_items(), vec![]);
}

#[cfg(not(debug_assertions))]
#[test]
fn normalize_mixed_inserts_and_removals() {
    let items = vec![
        // Will get an inserted output
        ResponseItem::FunctionCall {
            id: None,
            name: "f1".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "c1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        // Orphan output that should be removed
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "c2".to_string(),
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        // Will get an inserted custom tool output
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "t1".to_string(),
            name: "tool".to_string(),
            namespace: None,
            input: "{}".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        // Local shell call also gets an inserted function call output
        ResponseItem::LocalShellCall {
            id: None,
            call_id: Some("s1".to_string()),
            status: LocalShellStatus::Completed,
            action: LocalShellAction::Exec(LocalShellExecAction {
                command: vec!["echo".to_string()],
                timeout_ms: None,
                working_directory: None,
                env: None,
                user: None,
            }),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(
        h.raw_items(),
        vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "f1".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "c1".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "c1".to_string(),
                output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "t1".to_string(),
                name: "tool".to_string(),
                namespace: None,
                input: "{}".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "t1".to_string(),
                name: None,
                output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::LocalShellCall {
                id: None,
                call_id: Some("s1".to_string()),
                status: LocalShellStatus::Completed,
                action: LocalShellAction::Exec(LocalShellExecAction {
                    command: vec!["echo".to_string()],
                    timeout_ms: None,
                    working_directory: None,
                    env: None,
                    user: None,
                }),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "s1".to_string(),
                output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
}

#[test]
fn normalize_adds_missing_output_for_function_call_inserts_output() {
    let items = vec![ResponseItem::FunctionCall {
        id: None,
        name: "do_it".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "call-x".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);
    h.normalize_history(&default_input_modalities());
    assert_eq!(
        h.raw_items(),
        vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "do_it".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "call-x".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-x".to_string(),
                output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
}

#[test]
fn for_prompt_assigns_stable_id_to_synthetic_output_without_reordering_history() {
    let items = vec![
        ResponseItem::FunctionCall {
            id: Some(ResponseItemId::with_suffix("fc", "existing")),
            name: "do_it".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "call-x".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: Some(ResponseItemId::with_suffix("msg", "later")),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "later turn".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    let first = create_history_with_items(items.clone()).for_prompt(&default_input_modalities());
    let second = create_history_with_items(items).for_prompt(&default_input_modalities());

    assert_eq!(
        first, second,
        "repeated prompt projections should assign the same ID to the synthetic output"
    );
    let [
        ResponseItem::FunctionCall { .. },
        ResponseItem::FunctionCallOutput { id: Some(id), .. },
        ResponseItem::Message { .. },
    ] = first.as_slice()
    else {
        panic!("expected the synthetic output between its call and the later message");
    };
    assert!(
        id.starts_with("fco_"),
        "the synthetic function call output should use the Responses API output ID prefix"
    );
}

#[test]
fn prepared_prompt_cache_reuses_shared_items_across_non_history_changes() {
    let mut history = create_history_with_items(vec![agent_message("hello")]);
    let first = history
        .clone()
        .prepare_for_prompt(&default_input_modalities());
    history.set_token_info(history.token_info());
    let second = history.prepare_for_prompt(&default_input_modalities());

    assert!(Arc::ptr_eq(&first.shared_items(), &second.shared_items()));
    assert_eq!(first.fingerprint(), second.fingerprint());
}

#[test]
fn sampling_preparation_projects_stable_context_but_generic_preparation_fails_open() {
    let old_repository =
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nold\n</INSTRUCTIONS>";
    let current_repository =
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\ncurrent\n</INSTRUCTIONS>";
    let history = create_history_with_items(vec![
        user_msg(old_repository),
        user_msg("dynamic request"),
        user_msg(current_repository),
    ]);

    let generic = history
        .clone()
        .prepare_for_prompt(&default_input_modalities());
    let sampled = history
        .prepare_for_sampling_prompt(&default_input_modalities(), StableContextTarget::Sampling);

    assert_eq!(generic.items().len(), 3);
    assert!(generic.stable_context_manifest().fail_open());
    assert_eq!(sampled.items().len(), 2);
    assert!(sampled.stable_context_manifest().projection_enabled());
    assert!(!sampled.items().contains(&user_msg(old_repository)));
    assert!(sampled.items().contains(&user_msg(current_repository)));
}

#[test]
fn sampling_projection_reconstructs_current_variant_after_history_replacement() {
    let old_repository =
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nold\n</INSTRUCTIONS>";
    let current_repository =
        "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\ncurrent\n</INSTRUCTIONS>";
    let mut history = ContextManager::new();
    history.replace(vec![
        user_msg(old_repository),
        user_msg("compaction checkpoint summary"),
        user_msg(current_repository),
    ]);

    let sampled = history
        .prepare_for_sampling_prompt(&default_input_modalities(), StableContextTarget::Sampling);

    assert_eq!(sampled.items().len(), 2);
    assert!(!sampled.items().contains(&user_msg(old_repository)));
    assert!(sampled.items().contains(&user_msg(current_repository)));
    assert!(
        sampled
            .items()
            .contains(&user_msg("compaction checkpoint summary"))
    );
}

#[test]
fn prepared_prompt_cache_does_not_cross_divergent_clone_branches() {
    let mut left = create_history_with_items(vec![agent_message("shared")]);
    let mut right = left.clone();
    let left_only = agent_message("left only");
    let right_only = agent_message("right only");
    left.record_items([&left_only], TruncationPolicy::Tokens(10_000));
    right.record_items([&right_only], TruncationPolicy::Tokens(10_000));

    let left_prepared = left.prepare_for_prompt(&default_input_modalities());
    let right_prepared = right.prepare_for_prompt(&default_input_modalities());

    assert_eq!(
        left_prepared.items(),
        &[agent_message("shared"), agent_message("left only")]
    );
    assert_eq!(
        right_prepared.items(),
        &[agent_message("shared"), agent_message("right only")]
    );
    assert_ne!(left_prepared.fingerprint(), right_prepared.fingerprint());
}

#[test]
fn prepared_prompt_cache_does_not_extend_a_sibling_reasoning_branch() {
    let mut left = create_history_with_items(vec![agent_message("shared")]);
    let _base = left.clone().prepare_for_prompt(&default_input_modalities());
    let mut right = left.clone();
    let left_reasoning = reasoning_msg("left only");
    let right_reasoning = reasoning_msg("right only");
    left.record_items([&left_reasoning], TruncationPolicy::Tokens(10_000));
    right.record_items([&right_reasoning], TruncationPolicy::Tokens(10_000));

    let right_prepared = right.prepare_for_prompt(&default_input_modalities());

    assert_eq!(
        right_prepared.items(),
        &[agent_message("shared"), reasoning_msg("right only")]
    );
}

#[test]
fn prepared_prompt_cache_updates_only_for_safe_reasoning_append() {
    let mut history = create_history_with_items(vec![agent_message("hello")]);
    let first = history
        .clone()
        .prepare_for_prompt(&default_input_modalities());
    let reasoning = reasoning_msg("next");
    history.record_items([&reasoning], TruncationPolicy::Tokens(10_000));

    let cached = history
        .prepared_history
        .lock()
        .expect("prepared history lock")
        .clone()
        .expect("safe reasoning append should update the existing cache");
    assert_eq!(cached.projection_revision, history.projection_revision);
    assert_eq!(
        &cached.prepared.items()[..first.items().len()],
        first.items()
    );
    assert_ne!(cached.prepared.fingerprint(), first.fingerprint());

    let invalidating_item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "new boundary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    history.record_items([&invalidating_item], TruncationPolicy::Tokens(10_000));
    assert!(
        history
            .prepared_history
            .lock()
            .expect("prepared history lock")
            .is_none()
    );
}

#[test]
fn replacing_history_releases_obsolete_prepared_input() {
    let mut history = create_history_with_items(vec![agent_message("before")]);
    let prepared = history
        .clone()
        .prepare_for_prompt(&default_input_modalities());
    let shared = prepared.shared_items();
    let weak = Arc::downgrade(&shared);
    drop(shared);
    drop(prepared);

    history.replace(vec![agent_message("after")]);
    assert!(weak.upgrade().is_none());
}

#[test]
fn normalize_adds_missing_output_for_tool_search_call() {
    let items = vec![ResponseItem::ToolSearchCall {
        id: None,
        call_id: Some("search-call-x".to_string()),
        status: Some("completed".to_string()),
        execution: "client".to_string(),
        arguments: "{}".into(),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(
        h.raw_items(),
        vec![
            ResponseItem::ToolSearchCall {
                id: None,
                call_id: Some("search-call-x".to_string()),
                status: Some("completed".to_string()),
                execution: "client".to_string(),
                arguments: "{}".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ToolSearchOutput {
                id: None,
                call_id: Some("search-call-x".to_string()),
                status: "incomplete".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
                omitted_result_count: None,
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic]
fn normalize_adds_missing_output_for_custom_tool_call_panics_in_debug() {
    let items = vec![ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: "tool-x".to_string(),
        name: "custom".to_string(),
        namespace: None,
        input: "{}".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);
    h.normalize_history(&default_input_modalities());
}

#[cfg(debug_assertions)]
#[test]
#[should_panic]
fn normalize_adds_missing_output_for_local_shell_call_with_id_panics_in_debug() {
    let items = vec![ResponseItem::LocalShellCall {
        id: None,
        call_id: Some("shell-1".to_string()),
        status: LocalShellStatus::Completed,
        action: LocalShellAction::Exec(LocalShellExecAction {
            command: vec!["echo".to_string(), "hi".to_string()],
            timeout_ms: None,
            working_directory: None,
            env: None,
            user: None,
        }),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);
    h.normalize_history(&default_input_modalities());
}

#[cfg(debug_assertions)]
#[test]
#[should_panic]
fn normalize_removes_orphan_function_call_output_panics_in_debug() {
    let items = vec![ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "orphan-1".to_string(),
        output: FunctionCallOutputPayload::from_text("ok".to_string()),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);
    h.normalize_history(&default_input_modalities());
}

#[cfg(debug_assertions)]
#[test]
#[should_panic]
fn normalize_removes_orphan_custom_tool_call_output_panics_in_debug() {
    let items = vec![ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "orphan-2".to_string(),
        name: None,
        output: FunctionCallOutputPayload::from_text("ok".to_string()),
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);
    h.normalize_history(&default_input_modalities());
}

#[cfg(not(debug_assertions))]
#[test]
fn normalize_removes_orphan_client_tool_search_output() {
    let items = vec![ResponseItem::ToolSearchOutput {
        id: None,
        call_id: Some("orphan-search".to_string()),
        status: "completed".to_string(),
        execution: "client".to_string(),
        tools: Vec::new(),
        omitted_result_count: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(h.raw_items(), vec![]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic]
fn normalize_removes_orphan_client_tool_search_output_panics_in_debug() {
    let items = vec![ResponseItem::ToolSearchOutput {
        id: None,
        call_id: Some("orphan-search".to_string()),
        status: "completed".to_string(),
        execution: "client".to_string(),
        tools: Vec::new(),
        omitted_result_count: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);
    h.normalize_history(&default_input_modalities());
}

#[test]
fn normalize_keeps_server_tool_search_output_without_matching_call() {
    let items = vec![ResponseItem::ToolSearchOutput {
        id: None,
        call_id: Some("server-search".to_string()),
        status: "completed".to_string(),
        execution: "server".to_string(),
        tools: Vec::new(),
        omitted_result_count: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let mut h = create_history_with_items(items);

    h.normalize_history(&default_input_modalities());

    assert_eq!(
        h.raw_items(),
        vec![ResponseItem::ToolSearchOutput {
            id: None,
            call_id: Some("server-search".to_string()),
            status: "completed".to_string(),
            execution: "server".to_string(),
            tools: Vec::new(),
            omitted_result_count: None,
            internal_chat_message_metadata_passthrough: None,
        }]
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic]
fn normalize_mixed_inserts_and_removals_panics_in_debug() {
    let items = vec![
        ResponseItem::FunctionCall {
            id: None,
            name: "f1".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "c1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "c2".to_string(),
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "t1".to_string(),
            name: "tool".to_string(),
            namespace: None,
            input: "{}".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::LocalShellCall {
            id: None,
            call_id: Some("s1".to_string()),
            status: LocalShellStatus::Completed,
            action: LocalShellAction::Exec(LocalShellExecAction {
                command: vec!["echo".to_string()],
                timeout_ms: None,
                working_directory: None,
                env: None,
                user: None,
            }),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let mut h = create_history_with_items(items);
    h.normalize_history(&default_input_modalities());
}

#[test]
fn image_data_url_payload_does_not_dominate_message_estimate() {
    let payload = "A".repeat(100_000);
    let image_url = format!("data:image/png;base64,{payload}");
    let image_item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "Here is the screenshot".to_string(),
            },
            ContentItem::InputImage {
                image_url,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let text_only_item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "Here is the screenshot".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&image_item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&image_item);
    let expected = raw_len - payload.len() as i64 + RESIZED_IMAGE_BYTES_ESTIMATE;
    let text_only_estimated = estimate_response_item_model_visible_bytes(&text_only_item);

    assert_eq!(estimated, expected);
    assert!(estimated < raw_len);
    assert!(estimated > text_only_estimated);
}

#[test]
fn image_data_url_payload_does_not_dominate_function_call_output_estimate() {
    let payload = "B".repeat(50_000);
    let image_url = format!("data:image/png;base64,{payload}");
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-abc".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText {
                text: "Screenshot captured".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);
    let expected = raw_len - payload.len() as i64 + RESIZED_IMAGE_BYTES_ESTIMATE;

    assert_eq!(estimated, expected);
    assert!(estimated < raw_len);
}

#[test]
fn image_data_url_payload_does_not_dominate_custom_tool_call_output_estimate() {
    let payload = "C".repeat(50_000);
    let image_url = format!("data:image/png;base64,{payload}");
    let item = ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-js-repl".to_string(),
        name: None,
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText {
                text: "Screenshot captured".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);
    let expected = raw_len - payload.len() as i64 + RESIZED_IMAGE_BYTES_ESTIMATE;

    assert_eq!(estimated, expected);
    assert!(estimated < raw_len);
}

#[test]
fn non_base64_image_urls_are_unchanged() {
    let message_item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputImage {
            image_url: "https://example.com/foo.png".to_string(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let function_output_item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-1".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputImage {
                image_url: "file:///tmp/foo.png".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        estimate_response_item_model_visible_bytes(&message_item),
        serde_json::to_string(&message_item).unwrap().len() as i64
    );
    assert_eq!(
        estimate_response_item_model_visible_bytes(&function_output_item),
        serde_json::to_string(&function_output_item).unwrap().len() as i64
    );
}

#[test]
fn encrypted_function_output_uses_plaintext_byte_estimate() {
    let encrypted_content = "A".repeat(1_868);
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-encrypted".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: encrypted_content.clone(),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);
    let expected = raw_len - encrypted_content.len() as i64
        + estimate_encrypted_function_output_length(encrypted_content.len()) as i64;

    assert_eq!(estimated, expected);
}

#[test]
fn data_url_without_base64_marker_is_unchanged() {
    let item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputImage {
            image_url: "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg'/>".to_string(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        estimate_response_item_model_visible_bytes(&item),
        serde_json::to_string(&item).unwrap().len() as i64
    );
}

#[test]
fn non_image_base64_data_url_is_unchanged() {
    let payload = "C".repeat(4_096);
    let image_url = format!("data:application/octet-stream;base64,{payload}");
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-octet".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);

    assert_eq!(estimated, raw_len);
}

#[test]
fn mixed_case_data_url_markers_are_adjusted() {
    let payload = "F".repeat(1_024);
    let image_url = format!("DATA:image/png;BASE64,{payload}");
    let item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputImage {
            image_url,
            detail: Some(DEFAULT_IMAGE_DETAIL),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);
    let expected = raw_len - payload.len() as i64 + RESIZED_IMAGE_BYTES_ESTIMATE;

    assert_eq!(estimated, expected);
}

#[test]
fn multiple_inline_images_apply_multiple_fixed_costs() {
    let payload_one = "D".repeat(100);
    let payload_two = "E".repeat(200);
    let image_url_one = format!("data:image/png;base64,{payload_one}");
    let image_url_two = format!("data:image/jpeg;base64,{payload_two}");
    let item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "images".to_string(),
            },
            ContentItem::InputImage {
                image_url: image_url_one,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            ContentItem::InputImage {
                image_url: image_url_two,
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let payload_sum = (payload_one.len() + payload_two.len()) as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);
    let expected = raw_len - payload_sum + (2 * RESIZED_IMAGE_BYTES_ESTIMATE);

    assert_eq!(estimated, expected);
}

#[test]
fn original_detail_images_scale_with_dimensions() {
    // 2304x864 at 32px patches yields 72 * 27 = 1,944 patches.
    // The byte heuristic uses 4 bytes per token, so the replacement cost is 7,776 bytes.
    const EXPECTED_ORIGINAL_DETAIL_IMAGE_BYTES: i64 = 7_776;

    let width = 2304;
    let height = 864;
    let image = ImageBuffer::from_pixel(width, height, Rgba([12u8, 34, 56, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, ImageFormat::Png)
        .expect("encode png");
    let payload = BASE64_STANDARD.encode(bytes.get_ref());
    let image_url = format!("data:image/png;base64,{payload}");
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-original".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: Some(ImageDetail::Original),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);
    let expected = raw_len - payload.len() as i64 + EXPECTED_ORIGINAL_DETAIL_IMAGE_BYTES;

    assert_eq!(estimated, expected);
}

#[test]
fn original_detail_images_are_capped_at_max_patch_count() {
    // 3201x3201 at 32px patches yields 101 * 101 = 10,201 patches,
    // which exceeds the original-detail patch budget.
    let width = 3201;
    let height = 3201;
    let image = ImageBuffer::from_pixel(width, height, Luma([12u8]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, ImageFormat::Png)
        .expect("encode png");
    let payload = BASE64_STANDARD.encode(bytes.get_ref());
    let image_url = format!("data:image/png;base64,{payload}");
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-original-capped".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: Some(ImageDetail::Original),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);
    let capped_original_detail_image_bytes =
        i64::try_from(approx_bytes_for_tokens(ORIGINAL_IMAGE_MAX_PATCHES)).unwrap();
    let expected = raw_len - payload.len() as i64 + capped_original_detail_image_bytes;

    assert_eq!(estimated, expected);
}

#[test]
fn original_detail_webp_images_scale_with_dimensions() {
    // Same dimensions as the PNG case above, so the patch-based replacement cost is the same.
    const EXPECTED_ORIGINAL_DETAIL_IMAGE_BYTES: i64 = 7_776;

    let width = 2304;
    let height = 864;
    let image = ImageBuffer::from_pixel(width, height, Rgba([12u8, 34, 56, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, ImageFormat::WebP)
        .expect("encode webp");
    let payload = BASE64_STANDARD.encode(bytes.get_ref());
    let image_url = format!("data:image/webp;base64,{payload}");
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-original-webp".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: Some(ImageDetail::Original),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };

    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;
    let estimated = estimate_response_item_model_visible_bytes(&item);
    let expected = raw_len - payload.len() as i64 + EXPECTED_ORIGINAL_DETAIL_IMAGE_BYTES;

    assert_eq!(estimated, expected);
}

#[test]
fn text_only_items_unchanged() {
    let item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "Hello world, this is a response.".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    let estimated = estimate_response_item_model_visible_bytes(&item);
    let raw_len = serde_json::to_string(&item).unwrap().len() as i64;

    assert_eq!(estimated, raw_len);
}
