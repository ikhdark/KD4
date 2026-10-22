use super::*;
use base64::Engine as _;
use codex_protocol::models::ImageDetail;

fn text_item(role: &str, text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.into(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn prompt_for(items: Vec<ResponseItem>) -> Prompt {
    let input: Arc<[ResponseItem]> = items.into();
    Prompt {
        input: Arc::clone(&input),
        stable_context_fallback_input: Arc::clone(&input),
        tool_history_fallback_input: Arc::clone(&input),
        stable_context_tool_history_fallback_input: input,
        // These fixtures isolate the supplied input/schema budget, not the
        // production default's full canonical base instructions.
        base_instructions: BaseInstructions {
            text: String::new(),
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn final_admission_charges_schema_and_fallback_without_dropping_input() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.model_info.context_window = Some(1_000);
    turn.model_info.max_context_window = None;
    turn.model_info.effective_context_window_percent = 100;
    turn.model_info.auto_compact_token_limit = Some(900);
    Arc::make_mut(&mut turn.config).model_auto_compact_token_limit_scope =
        AutoCompactTokenLimitScope::Total;
    let mut prompt = prompt_for(vec![text_item("user", "a".repeat(2_400))]);
    admit_prepared_prompt(&session, &turn, &prompt)
        .await
        .unwrap();
    let original = Arc::clone(&prompt.input);
    prompt.output_schema =
        Some(serde_json::json!({"type":"object", "description":"s".repeat(4_000)}));
    assert!(matches!(
        admit_prepared_prompt(&session, &turn, &prompt).await,
        Err(CodexErr::ContextWindowExceeded)
    ));
    assert_eq!(prompt.input, original);
    prompt.output_schema = None;
    prompt.stable_context_tool_history_fallback_input =
        vec![text_item("user", "b".repeat(4_000))].into();
    assert!(matches!(
        admit_prepared_prompt(&session, &turn, &prompt).await,
        Err(CodexErr::ContextWindowExceeded)
    ));
    assert_eq!(prompt.input, original);
}

#[tokio::test]
async fn final_admission_uses_loaded_original_image_geometry() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.model_info.context_window = Some(16_000);
    turn.model_info.max_context_window = None;
    turn.model_info.effective_context_window_percent = 95;
    turn.model_info.auto_compact_token_limit = Some(14_400);
    turn.model_info.use_responses_lite = false;
    Arc::make_mut(&mut turn.config).model_auto_compact_token_limit_scope =
        AutoCompactTokenLimitScope::Total;
    let image_item = |url| ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputImage {
            image_url: url,
            detail: Some(ImageDetail::Original),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let pending = prompt_for(vec![
        text_item("user", "a".repeat(32_000)),
        image_item("data:image/png;base64,".into()),
    ]);
    admit_prepared_prompt(&session, &turn, &pending)
        .await
        .unwrap();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(3_200, 3_200)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png.into_inner())
    );
    let loaded = prompt_for(vec![text_item("user", "a".repeat(32_000)), image_item(url)]);
    assert!(matches!(
        admit_prepared_prompt(&session, &turn, &loaded).await,
        Err(CodexErr::ContextWindowExceeded)
    ));
}

#[tokio::test]
async fn body_after_prefix_admission_charges_growth_only_after_an_observed_prefix() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.model_info.context_window = Some(100_000);
    turn.model_info.max_context_window = None;
    turn.model_info.effective_context_window_percent = 100;
    turn.model_info.auto_compact_token_limit = Some(40);
    Arc::make_mut(&mut turn.config).model_auto_compact_token_limit_scope =
        AutoCompactTokenLimitScope::BodyAfterPrefix;
    let prompt = prompt_for(vec![text_item("user", "a".repeat(400))]);
    // A recompute estimate covers history only. The first request of the window
    // establishes the prefix, so it is not charged as body growth.
    session
        .state
        .lock()
        .await
        .set_auto_compact_window_estimated_prefill(/*tokens*/ 10);
    admit_prepared_prompt(&session, &turn, &prompt)
        .await
        .unwrap();
    // After the server observes the window's first request input, later growth
    // counts against the scoped limit.
    session
        .state
        .lock()
        .await
        .ensure_auto_compact_window_server_prefill_from_usage(
            &codex_protocol::protocol::TokenUsage {
                input_tokens: 10,
                ..Default::default()
            },
        );
    assert!(matches!(
        admit_prepared_prompt(&session, &turn, &prompt).await,
        Err(CodexErr::ContextWindowExceeded)
    ));
}

#[tokio::test]
async fn calibration_only_discounts_a_matching_observed_prefix() {
    let prompt = prompt_for(vec![text_item("user", "a".repeat(4_000))]);
    let basis = prompt.calibration_basis(Arc::clone(&prompt.input));
    let measured = basis.local_tokens / 2;
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let mut calibration = std::mem::take(&mut session.state.lock().await.prompt_token_calibration);
    calibration.observe_prompt(
        "provider",
        "model",
        basis.clone(),
        Some(measured as u64),
        Some(measured as u64),
    );
    assert_eq!(
        calibration.estimate_prompt("provider", "model", &basis),
        measured
    );
    let grown = prompt_for(vec![
        prompt.input[0].clone(),
        text_item("user", "d".repeat(800)),
    ]);
    let grown_basis = grown.calibration_basis(Arc::clone(&grown.input));
    assert_eq!(
        calibration.estimate_prompt("provider", "model", &grown_basis),
        measured + 200
    );
    let mut changed_schema = grown;
    changed_schema.output_schema =
        Some(serde_json::json!({"type":"object", "description":"schema".repeat(1_000)}));
    let changed = changed_schema.calibration_basis(Arc::clone(&changed_schema.input));
    assert_eq!(
        calibration.estimate_prompt("provider", "model", &changed),
        changed.local_tokens
    );
    assert_eq!(
        calibration.estimate_prompt("other", "model", &basis),
        basis.local_tokens
    );
    calibration.observe_prompt("provider", "model", basis.clone(), None, None);
    assert_eq!(
        calibration.estimate_prompt("provider", "model", &basis),
        basis.local_tokens
    );
    // Late/missing diagnostics do not erase a matching provider input floor.
    calibration.observe_prompt("provider", "model", basis.clone(), None, Some(2_000));
    assert_eq!(
        calibration.estimate_prompt("provider", "model", &basis),
        2_000
    );

    let mut lite_items = vec![text_item("developer", "embedded base".into())];
    lite_items.push(ResponseItem::AdditionalTools {
        id: None,
        role: "developer".into(),
        tools: Vec::new(),
    });
    lite_items.extend(prompt.input.iter().cloned());
    let lite = prompt.calibration_basis_with_prefix(lite_items.into(), 2);
    assert_eq!(lite.local_tokens, basis.local_tokens);
    assert!(lite.extends(&basis));
}

#[tokio::test]
async fn missing_usage_waits_for_required_history_but_not_auxiliary_delivery() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let recorder = OrderedResponseItemRecorder::default();
    let (release_required, required) = tokio::sync::oneshot::channel();
    let (release_auxiliary, auxiliary) = tokio::sync::oneshot::channel();
    recorder.block_required_persistence_for_test(required).await;
    recorder
        .block_auxiliary_persistence_for_test(auxiliary)
        .await;
    let _required = recorder
        .enqueue(
            Arc::clone(&session),
            Arc::clone(&turn),
            text_item("assistant", "a".repeat(4_000)),
            Vec::new(),
            None,
        )
        .await;
    let mut accounting = Box::pin(async {
        recorder.flush_required().await.unwrap();
        session.record_token_usage_info(&turn, None).await.unwrap();
    });
    assert!(futures::poll!(accounting.as_mut()).is_pending());
    release_required.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), accounting)
        .await
        .unwrap();
    let history = session.clone_history().await;
    assert_eq!(
        session.get_context_tokens_for_turn(&turn).await,
        history.estimate_token_count(&turn).unwrap()
    );
    assert_eq!(
        history.token_info().unwrap().total_token_usage.total_tokens,
        0
    );
    let mut flush = Box::pin(recorder.flush());
    assert!(futures::poll!(flush.as_mut()).is_pending());
    release_auxiliary.send(()).unwrap();
    flush.await.unwrap();
}
