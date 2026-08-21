use std::sync::Arc;

use codex_core::ForkSnapshot;
use codex_core::NewThread;
use codex_core::parse_turn_item;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_thread_twice_drops_to_first_message() {
    skip_if_no_network!();

    // Start a mock server that completes three turns.
    let server = MockServer::start().await;
    let sse = sse(vec![ev_response_created("resp"), ev_completed("resp")]);
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse.clone(), "text/event-stream");

    // Expect three calls to /v1/responses – one per user input.
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(first)
        .expect(3)
        .mount(&server)
        .await;

    let mut builder = test_codex();
    let test = builder.build(&server).await.expect("create conversation");
    let codex = test.codex.clone();
    let thread_manager = test.thread_manager.clone();
    let config_for_fork = test.config.clone();

    // Send three user messages; wait for three completed turns.
    for text in ["first", "second", "third"] {
        codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await
            .unwrap();
        let _ = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    }

    // Request history from the base conversation to obtain rollout path.
    let base_path = codex.rollout_path().expect("rollout path");

    // GetHistory flushes before returning the path; no wait needed.

    // Compute expected prefixes after each fork by truncating base rollout
    // strictly before the nth user input (0-based).
    let base_items = read_rollout_items(&base_path);
    let find_user_input_positions = |items: &[RolloutItem]| -> Vec<usize> {
        let mut pos = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if let RolloutItem::ResponseItem(response_item) = it
                && let Some(TurnItem::UserMessage(_)) = parse_turn_item(response_item)
            {
                // Consider any user message as an input boundary; recorder stores both EventMsg and ResponseItem.
                // We specifically look for input items, which are represented as ContentItem::InputText.
                pos.push(i);
            }
        }
        pos
    };
    let user_inputs = find_user_input_positions(&base_items);

    // After cutting at nth user input (n=1 → second user message), cut strictly before that input.
    let cut1 = user_inputs.get(1).copied().unwrap_or(0);
    let expected_after_first: Vec<RolloutItem> = base_items[..cut1].to_vec();

    // After dropping again (n=1 on fork1), compute expected relative to fork1's rollout.

    // Fork once with n=1 → drops the last user input and everything after.
    let NewThread {
        thread: codex_fork1,
        ..
    } = thread_manager
        .fork_thread(
            ForkSnapshot::TruncateBeforeNthUserMessage(1),
            config_for_fork.clone(),
            base_path.clone(),
            /*thread_source*/ None,
            /*parent_trace*/ None,
        )
        .await
        .expect("fork 1");

    let fork1_path = codex_fork1.rollout_path().expect("rollout path");

    // GetHistory on fork1 flushed; the file is ready.
    let fork1_items = read_rollout_items(&fork1_path);
    assert_fork_rollout(&fork1_items, &expected_after_first);

    // Fork again with n=0 → drops the (new) last user message, leaving only the first.
    let NewThread {
        thread: codex_fork2,
        ..
    } = thread_manager
        .fork_thread(
            ForkSnapshot::TruncateBeforeNthUserMessage(0),
            config_for_fork.clone(),
            fork1_path.clone(),
            /*thread_source*/ None,
            /*parent_trace*/ None,
        )
        .await
        .expect("fork 2");

    let fork2_path = codex_fork2.rollout_path().expect("rollout path");
    // GetHistory on fork2 flushed; the file is ready.
    let fork1_items = read_rollout_items(&fork1_path);
    let fork1_user_inputs = find_user_input_positions(&fork1_items);
    let cut_last_on_fork1 = fork1_user_inputs
        .get(fork1_user_inputs.len().saturating_sub(1))
        .copied()
        .unwrap_or(0);
    let expected_after_second: Vec<RolloutItem> = fork1_items[..cut_last_on_fork1].to_vec();
    let fork2_items = read_rollout_items(&fork2_path);
    assert_fork_rollout(&fork2_items, &expected_after_second);
}

#[test]
fn fork_thread_from_history_does_not_require_source_rollout_path() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("build fork-thread test runtime");
    runtime.block_on(async {
        skip_if_no_network!();

        let server = MockServer::start().await;
        let initial_sse = sse(vec![
            ev_response_created("resp-source"),
            ev_reasoning_item("reason-fork", &["fork summary"], &["fork detail"]),
            ev_assistant_message("msg-source", "source complete"),
            ev_completed("resp-source"),
        ]);
        let forked_sse = sse(vec![
            ev_response_created("resp-forked"),
            ev_assistant_message("msg-forked", "fork complete"),
            ev_completed("resp-forked"),
        ]);
        let request_log = mount_sse_sequence(&server, vec![initial_sse, forked_sse]).await;

        let mut builder = test_codex();
        let test = builder.build(&server).await.expect("create conversation");
        let codex = test.codex.clone();
        let thread_manager = test.thread_manager.clone();

        codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "fork me from stored history".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await
            .unwrap();
        let _ = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

        let source_path = codex.rollout_path().expect("source rollout path");
        let source_items = read_rollout_items(&source_path);
        assert!(
            contains_reasoning(&source_items, "reason-fork"),
            "source rollout must retain the full reasoning record"
        );
        let NewThread {
            thread: forked_thread,
            ..
        } = thread_manager
            .fork_thread_from_history(
                ForkSnapshot::Interrupted,
                test.config.clone(),
                InitialHistory::Resumed(ResumedHistory {
                    conversation_id: test.session_configured.thread_id,
                    history: Arc::new(source_items.clone()),
                    rollout_path: None,
                }),
                /*thread_source*/ None,
                /*parent_trace*/ None,
                /*supports_openai_form_elicitation*/ false,
            )
            .await
            .expect("fork from stored history");

        let forked_path = forked_thread.rollout_path().expect("forked rollout path");
        let forked_items = read_rollout_items(&forked_path);
        assert!(
            contains_reasoning(&forked_items, "reason-fork"),
            "forked rollout must retain the full reasoning record"
        );
        let forked_item_values = forked_items
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect::<Vec<_>>();
        let source_item_values = source_items
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect::<Vec<_>>();
        assert!(
            forked_item_values.starts_with(&source_item_values),
            "forked history should start with the supplied source history"
        );

        forked_thread
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "continue the fork".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await
            .unwrap();
        let _ = wait_for_event(&forked_thread, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

        let requests = request_log.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].inputs_of_type("reasoning").is_empty(),
            "the next forked request must project out reasoning from completed instruction groups"
        );
    });
}

fn contains_reasoning(items: &[RolloutItem], expected_id: &str) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::ResponseItem(ResponseItem::Reasoning { id: Some(id), .. })
                if id.as_str() == expected_id
        )
    })
}

fn assert_fork_rollout(actual: &[RolloutItem], expected_copied_prefix: &[RolloutItem]) {
    assert_eq!(
        actual.len(),
        expected_copied_prefix.len() + 1,
        "a fork should append exactly one authoritative child-settings snapshot"
    );
    pretty_assertions::assert_eq!(
        serde_json::to_value(&actual[..expected_copied_prefix.len()])
            .expect("actual fork rollout prefix should serialize"),
        serde_json::to_value(expected_copied_prefix)
            .expect("expected fork rollout prefix should serialize")
    );
    assert!(
        matches!(
            actual.last(),
            Some(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(_)))
        ),
        "fork rollout should end with the child's authoritative thread settings"
    );
}

fn read_rollout_items(path: &std::path::Path) -> Vec<RolloutItem> {
    let read_message = format!("failed to read rollout file {}", path.display());
    let text = std::fs::read_to_string(path).expect(&read_message);
    let mut items: Vec<RolloutItem> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parse_json_message = format!("failed to parse rollout JSON line `{line}`");
        let v: serde_json::Value = serde_json::from_str(line).expect(&parse_json_message);
        let parse_line_message = format!("failed to parse rollout line `{line}`");
        let rl: RolloutLine = serde_json::from_value(v).expect(&parse_line_message);
        match rl.item {
            RolloutItem::SessionMeta(_) => {}
            other => items.push(other),
        }
    }
    items
}
