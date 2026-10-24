use super::*;

#[tokio::test]
async fn repeated_searches_execute_and_observe_changed_inputs() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let command = ["rg", "--no-config", "--no-ignore-global", "--no-ignore-parent", "-f", "patterns.txt", "input.txt"].map(str::to_owned);
    let key = CommandAttemptKey::new("exec_command", "local", root.to_string_lossy(), &command)
        .with_search_no_match_eligibility(true);
    let ledger = CommandExecutionLedger::default();
    std::fs::write(root.join("input.txt"), "present\n").unwrap();
    for (pattern, expected_exit) in [("absent", 1), ("absent", 1), ("present", 0)] {
        std::fs::write(root.join("patterns.txt"), pattern).unwrap();
        ledger.begin_attempt(&key, false).await.expect("every requested search executes");
        let output = std::process::Command::new(&command[0])
            .args(&command[1..]).current_dir(root).output().unwrap();
        assert_eq!(output.status.code(), Some(expected_exit));
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout.is_empty(), expected_exit == 1);
        ledger.record_exit(&key, expected_exit).await;
    }
    assert_eq!(ledger.started_attempt_count().await, 3);
    assert!(key.is_search_no_match(Some(1)));
    assert!(!key.is_search_no_match(Some(2)));
    assert!(!key.is_search_no_match(None));
    for variable in ["RIPGREP_CONFIG_PATH", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        let contextual = key.clone().with_search_environment(&HashMap::from([(variable.into(), "config".into())]));
        assert!(!contextual.is_search_no_match(Some(1)));
    }
}

#[tokio::test]
async fn retired_search_cache_documents_do_not_suppress_execution() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    assert!(std::process::Command::new("git").args(["init", "--quiet"]).current_dir(&root).status().unwrap().success());
    let home = fixture.path().join("home");
    let cache = home.join("command-execution-cache/thread.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let identity = crate::git_workspace::capture_workspace_evidence_identity(&root).await.unwrap();
    std::fs::write(&cache, serde_json::to_vec(&serde_json::json!({
        "schema_version": 5, "workspace_identity": identity,
        "search_misses": [{"old":"miss"}], "input_failures": {}
    })).unwrap()).unwrap();
    let ledger = CommandExecutionLedger::load_or_new(home, "thread".into(), &root).await;
    assert!(ledger.state.lock().await.repository.observed_workspace_identity.is_none());
    ledger.observe_repository_revision("turn", 0).await;
    let key = CommandAttemptKey::new("exec_command", "local", root.to_string_lossy(), &["rg".into(), "needle".into(), ".".into()])
        .with_search_no_match_eligibility(true);
    for _ in 0..2 {
        ledger.begin_attempt(&key, false).await.unwrap();
        ledger.record_exit(&key, 1).await;
    }
    ledger.finish_turn("turn").await;
    ledger.persist_cache_after_terminal().await;
    assert_eq!(ledger.cache_commit_count.load(Ordering::Relaxed), 1);
    let document: serde_json::Value = serde_json::from_slice(&std::fs::read(cache).unwrap()).unwrap();
    assert_eq!(document["schema_version"], 6);
    assert!(document.get("search_misses").is_none());
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
