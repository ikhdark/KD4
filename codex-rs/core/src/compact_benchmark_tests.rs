// Production regression coverage plus the remaining opt-in summary-archiving benchmark.

#[tokio::test]
#[ignore = "targeted compaction baseline/candidate benchmark"]
#[expect(clippy::print_stdout, reason = "emits the opt-in benchmark report")]
async fn compaction_benchmark_summary_overflow() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let previous = format!(
        "{SUMMARY_PREFIX}\n## Goal\nFinish the task; do not deploy.\n\n## Current state\nOwner: core/src/compact.rs\n\n## Completed work\nParser repaired.\n\n## Unresolved work\nWire the caller.\n\n## Evidence\nUNCHANGED_PROOF: focused parser tests passed at source hash abc; recover artifact old-proof /items/0.\n\n## Next action\nInspect the caller."
    );
    let suffix = format!(
        "## Evidence\nNEW_PROOF: caller inspected.\n{}",
        "new evidence ".repeat(800)
    );
    let complete = format!("{previous}\n\n{suffix}");
    let mut baseline_us = Vec::new();
    let mut archive_us = Vec::new();
    let mut receipt_tokens = 0;
    for _ in 0..9 {
        let started = Instant::now();
        let bounded = validated_compaction_summary(Some(&previous), &suffix, true).unwrap();
        baseline_us.push(started.elapsed().as_micros());
        assert!(!bounded.contains("UNCHANGED_PROOF"));
        assert!(bounded.contains("NEW_PROOF"));
        assert!(bounded.contains("do not deploy"));
        assert!(approx_token_count(&bounded) <= COMPACT_TASK_STATE_MAX_TOKENS);

        // Exercise the same recovery path used before installing a bounded checkpoint.
        let started = Instant::now();
        let (preserved, receipt) =
            preserve_bounded_summary(&session, Some(&previous), &suffix, bounded.clone())
                .await
                .unwrap();
        assert_eq!(preserved, bounded);
        let receipt = receipt.expect("truncated evidence requires exact recovery");
        archive_us.push(started.elapsed().as_micros());
        let value: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        let bytes = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
            &turn.config.codex_home,
            &session.thread_id.to_string(),
            value["artifact_id"].as_str().unwrap(),
            1024 * 1024,
        )
        .await
        .unwrap();
        let recovered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(recovered["items"][0]["checkpoint"], complete);
        receipt_tokens = approx_token_count(&receipt);
    }
    baseline_us.sort_unstable();
    archive_us.sort_unstable();
    println!(
        "COMPACTION_BENCH {}",
        json!({
            "case": "summary_overflow", "samples": 9,
            "baseline_old_proof_inline": false, "candidate_exact_recovery": true,
            "baseline_p50_us": baseline_us[4], "additional_archive_p50_us": archive_us[4],
            "additional_receipt_estimated_tokens": receipt_tokens,
            "archived_text_bytes": complete.len(), "model_requests": 0
        })
    );
}

#[tokio::test]
async fn repeated_local_compaction_preserves_exact_recovery() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let exact = format!(
        "{}OMITTED_AGENT_SENTINEL{}",
        "worker evidence ".repeat(5000),
        " tail".repeat(5000)
    );
    let source = vec![agent_message(&exact)];
    let (mut first, _, _, _, omitted) = build_bounded_unresolved_input_history(&source);
    assert!(omitted);
    let sidecar = persist_compaction_text_recovery(&session, &source, omitted)
        .await
        .unwrap()
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&sidecar).unwrap();
    let artifact_id = metadata["artifact_id"].as_str().unwrap().to_string();
    assert_eq!(
        session
            .clone_history()
            .await
            .tool_history_state()
            .artifact_references()
            .get(&artifact_id),
        Some(&(
            metadata["canonical_bytes"].as_u64().unwrap(),
            metadata["canonical_sha256"].as_str().unwrap().to_string()
        ))
    );
    let mut summary = summary_message("settled state");
    if let ResponseItem::Message { content, .. } = &mut summary {
        content.push(ContentItem::InputText { text: sidecar });
    }
    first.push(summary);
    session
        .record_conversation_items(&turn, &first)
        .await
        .unwrap();
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    for _ in 0..3 {
        let history = session.clone_history().await;
        let (_, _, _, _, omitted_again) =
            build_bounded_unresolved_input_history(history.raw_items());
        assert!(
            !omitted_again,
            "must preserve old recovery without creating another artifact"
        );
        run_compact_task_inner_impl(
            Arc::clone(&session),
            Arc::clone(&turn),
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
        let installed = session.clone_history().await;
        assert!(
            serde_json::to_string(installed.raw_items())
                .unwrap()
                .contains(&artifact_id)
        );
        assert_eq!(
            installed.tool_history_state().artifact_references().len(),
            1
        );
    }
    assert!(
        turn.turn_timing_state
            .complete_snapshot()
            .protocol_timing()
            .model_requests
            .is_empty()
    );

    session.flush_tool_history_persistence().await.unwrap();
    let thread = session.thread_id.to_string();
    let crate::tool_history::ToolHistoryLoadOutcome::Loaded(restored) =
        crate::tool_history::load_tool_history_state(&turn.config.codex_home, &thread).await
    else {
        panic!("compaction recovery origin must survive ledger reload");
    };
    assert!(restored.artifact_references().contains_key(&artifact_id));
    let bytes = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
        &turn.config.codex_home,
        &thread,
        &artifact_id,
        1024 * 1024,
    )
    .await
    .unwrap();
    let recovered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(recovered["items"][0]["content"][0]["text"], exact);

    let (forked, dropped) = crate::tool_history::remint_tool_history_state_for_fork(
        &turn.config.codex_home,
        &thread,
        "recovery-child",
        restored,
    )
    .await;
    assert_eq!(dropped, 0);
    assert_eq!(forked.artifact_references().len(), 1);
    let child_id = forked.artifact_references().into_keys().next().unwrap();
    let child_bytes = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
        &turn.config.codex_home,
        "recovery-child",
        &child_id,
        1024 * 1024,
    )
    .await
    .unwrap();
    assert_eq!(child_bytes, bytes);
}

#[test]
fn compaction_pin_replacement_preserves_user_and_legacy_messages() {
    let payload = json!({
        "version": 1, "kind": "tool_history_artifact_pins",
        "artifacts": [{ "artifact_id": "preserve-this-user-text" }],
    })
    .to_string();
    let user = user_message(&payload);
    let mut legacy = user.clone();
    legacy.set_id(Some(ResponseItemId::new("msg")));
    let mut owned = user.clone();
    owned.set_id(Some(ResponseItemId::new("msg_compaction_artifact_pins")));
    let source = vec![user.clone(), legacy.clone(), owned];
    assert_eq!(
        task_compaction_items(&source),
        vec![user.clone(), legacy.clone()]
    );
    assert_eq!(unresolved_compaction_items(&source), vec![user, legacy]);

    // Switching from local to remote compaction must not duplicate the local pin attachment.
    let text = format!("{SUMMARY_PREFIX}\nKeep the original requirement.");
    let mut summary = compaction_summary_item_with_artifact_pins(text.clone(), Some(payload));
    let recovery =
        json!({"kind": "local_compaction_text_recovery", "artifact_id": "exact-input"}).to_string();
    if let ResponseItem::Message { content, .. } = &mut summary {
        content.push(ContentItem::InputText {
            text: recovery.clone(),
        });
    }
    let retained = task_compaction_items(&[summary]);
    let ResponseItem::Message { content, .. } = &retained[0] else {
        panic!("summary retained");
    };
    assert_eq!(
        content,
        &vec![
            ContentItem::InputText { text },
            ContentItem::InputText { text: recovery }
        ]
    );
}
