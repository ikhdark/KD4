use super::*;

#[tokio::test]
async fn completed_process_recovery_keeps_owned_validation_signal_and_rejects_reused_identity() {
    let ledger = CommandExecutionLedger::default();
    let cwd = if cfg!(windows) { "C:/repo" } else { "/repo" };
    let command = CommandAttemptKey::new("exec_command","local",cwd,&["cargo check -p consumer".to_string()]);
    ledger.begin_attempt(&command,false).await.unwrap();
    ledger.track_running_process(41,command,RawOutputArtifact::unavailable("fixture")).await.unwrap();
    let running = ledger.running_process(41).await.unwrap();
    let parent = running.parent_tool_execution_id.0.clone();
    ledger.record_terminal_facts(41,&parent,running.incarnation,Some(0),None,false).await;
    let signal = serde_json::json!({"outcome":"success","command_validation":{"validation":true,"proof":true,"tests":false,
        "scope":{"environment_id":"local","paths":[{"path":"consumer.rs","recursive":false}]}}});
    ledger.retain_terminal_validation_signal(41,&parent,uuid::Uuid::new_v4(),signal.clone()).await;
    ledger.retain_terminal_validation_signal(41,&parent,running.incarnation,signal.clone()).await;
    assert!(ledger.mark_running_process_completed(41,0).await.accepted());
    assert_eq!(ledger.retire_completed_process(running.execution_id,&running.parent_tool_execution_id).await,CompletionApplyResult::Applied);
    let recovered = ledger.completed_process_result(41,running.incarnation).await.unwrap();
    assert_eq!(recovered["_sampling_request_signal"],signal);
    assert_eq!(recovered["output"],"");
    assert!(ledger.completed_process_result(41,uuid::Uuid::new_v4()).await.is_none());
}
