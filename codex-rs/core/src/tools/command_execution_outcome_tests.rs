use super::*;

#[tokio::test]
async fn exit_one_without_verified_evidence_never_populates_search_cache() {
    let ledger = CommandExecutionLedger::default();
    let key = CommandAttemptKey::new("exec_command", "local", "/repo", &["rg needle src".into()])
        .with_search_narrowing(
            "turn",
            "repo",
            Some(RgSearchNarrowing {
                breadth: RgSearchBreadth::Narrow,
                query_identity: "needle".into(),
                search_identity: "needle:src".into(),
                scope_identity: "src".into(),
                parent_scope_identity: Some("repo".into()),
                scope_state_identity: Some("scope".into()),
                state_paths: Vec::new(),
                can_record_miss: true,
            }),
        );
    ledger.record_exit(&key, 1).await;
    ledger.begin_attempt(&key, false).await.unwrap();
    assert!(ledger.state.lock().await.search.misses.is_empty());
    assert!(
        ledger
            .state
            .lock()
            .await
            .search
            .allowed_expansions
            .is_empty()
    );
    ledger.record_verified_search_result(&key, true).await;
    assert!(
        ledger
            .begin_attempt(&key, false)
            .await
            .unwrap_err()
            .is_search_miss()
    );
    ledger.record_verified_search_result(&key, false).await;
    ledger.begin_attempt(&key, false).await.unwrap();
}

#[tokio::test]
async fn compact_completion_retains_late_failure_and_rejects_weaker_success() {
    for code in [Some(101), None, Some(0)] {
        let ledger = CommandExecutionLedger::default();
        let key = CommandAttemptKey::new("exec_command", "local", "/repo", &["save".into()]);
        ledger
            .track_running_process(42, key, RawOutputArtifact::unavailable("fixture"))
            .await
            .unwrap();
        let running = ledger.running_process(42).await.unwrap();
        ledger
            .mark_process_exited(
                42,
                running.execution_id,
                &running.parent_tool_execution_id,
                code,
            )
            .await;
        ledger
            .retire_completed_process(running.execution_id, &running.parent_tool_execution_id)
            .await;
        ledger
            .record_terminal_facts(
                42,
                &running.parent_tool_execution_id.0,
                uuid::Uuid::new_v4(),
                Some(0),
                None,
                false,
            )
            .await;
        assert_eq!(
            ledger
                .completed_process_result(42, running.incarnation)
                .await
                .unwrap()["terminal_facts_available"],
            false
        );
        ledger
            .record_terminal_facts(
                42,
                &running.parent_tool_execution_id.0,
                running.incarnation,
                code,
                Some("late harness failure"),
                false,
            )
            .await;
        ledger
            .record_terminal_facts(
                42,
                &running.parent_tool_execution_id.0,
                running.incarnation,
                code,
                None,
                false,
            )
            .await;
        let result = ledger
            .completed_process_result(42, running.incarnation)
            .await
            .unwrap();
        assert_eq!(result["exit_code"], serde_json::json!(code));
        assert_eq!(result["success"], false);
        assert_eq!(result["error"], "late harness failure");
        assert!(
            ledger
                .completed_process_result(42, uuid::Uuid::new_v4())
                .await
                .is_none()
        );
    }
}
