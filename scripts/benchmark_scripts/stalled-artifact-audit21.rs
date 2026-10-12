// UNCOMPILED DIAGNOSTIC, NOT REGISTERED IN TEST SUITE.
// Intended insertion: raw_artifact_queue_tests in
// C:/Users/kuh/Desktop/kd4/codex-rs/core/src/unified_exec/process.rs.
// Authored during audit 21; this diagnostic has not been compiled or executed.
// This source is a pending reproduction, not evidence that the assertion fails.

#[test]
fn terminal_packet_does_not_claim_stalled_artifact_is_complete() {
    use crate::tools::context::ToolOutput;
    use crate::tools::context::ToolPayload;
    use crate::unified_exec::WriteStdinRequest;
    use crate::unified_exec::process_tests::remote_process;
    use crate::unified_exec::process_tests::store_process_for_test;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            temp.path(), "stalled-terminal-artifact", b"",
        ).await;
        let artifact_path = match &artifact {
            RawOutputArtifact::Stored { path, .. } => path.clone(),
            other => panic!("fixture artifact unavailable: {other:?}"),
        };
        let mock = remote_process(codex_exec_server::WriteStatus::Accepted, None).await;
        let ProcessHandle::ExecServer(handle) = &mock.process_handle else {
            unreachable!("remote fixture")
        };
        let process = Arc::new(UnifiedExecProcess::new(
            ProcessHandle::ExecServer(Arc::clone(handle)),
            SandboxType::None, None, Some(artifact),
        ));
        let manager = &session.services.unified_exec_manager;
        store_process_for_test(manager, &session, &turn, 8_181, Arc::clone(&process)).await;
        manager.process_store.lock().await.processes.get_mut(&8_181).unwrap().tty = false;

        // Hold only this runtime's blocking I/O worker, not the artifact
        // state mutex. EOF and the model packet can still be observed.
        let (release, released) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = released.recv_timeout(Duration::from_secs(20));
        });
        running.await.expect("blocking worker occupied");
        let (stdout_tx, stdout_rx) = mpsc::channel(1);
        let (stderr_tx, stderr_rx) = mpsc::channel(1);
        let output_task = UnifiedExecProcess::spawn_local_output_task(
            stdout_rx, stderr_rx, process.output_handles(), process.output_tx.clone(),
            process.raw_output_artifact.clone(), process.artifact_ready.clone(),
        );
        let expected = vec![b'x';
            crate::tools::command_output_artifact::LAZY_RAW_OUTPUT_ARTIFACT_THRESHOLD_BYTES + 1];
        stdout_tx.send(expected.clone()).await.expect("write fixture output");
        drop(stdout_tx);
        drop(stderr_tx);
        let closed = process.output_closed_notify.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if !process.output_is_closed() {
            tokio::time::timeout(Duration::from_secs(1), &mut closed)
                .await.expect("EOF must not wait for storage");
        }
        process.signal_exit(Some(0));
        assert!(!process.artifact_ready.is_cancelled());
        let observed = tokio::time::timeout(Duration::from_secs(5), manager.write_stdin(
            WriteStdinRequest {
                process_id: 8_181, input: "", yield_time_ms: 250,
                max_output_tokens: Some(100),
                truncation_policy: codex_utils_output_truncation::TruncationPolicy::Tokens(100),
                nested_deadline: None,
            },
        )).await.expect("bounded readiness observation").expect("terminal result");
        let packet = observed.code_mode_result(&ToolPayload::Function {
            arguments: "{}".into(),
        });
        let storage_was_ready = process.artifact_ready.is_cancelled();

        // Release and join all fixture work before the regression assertion.
        // The command bytes were observed, but the retained sink had none.
        drop(release);
        blocker.await.expect("blocking worker released");
        output_task.await.expect("artifact writer finishes");
        mock.terminate_confirmed().await.expect("close mock owner");
        assert_eq!(tokio::fs::read(artifact_path).await.unwrap(), expected);
        assert!(!storage_was_ready);
        assert_eq!(packet["process_exited"], true);
        assert!(packet.get("session_id").is_none());
        assert_eq!(packet["cumulative_streams_complete"], true);
        assert_eq!(packet["raw_output_artifact_bytes"], 0);
        assert_ne!(packet["retained_artifact_complete"], true,
            "process EOF is not proof that the retained artifact writer caught up");
    });
}
