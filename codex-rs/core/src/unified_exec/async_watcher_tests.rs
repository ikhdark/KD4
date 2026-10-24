use super::append_output_loss_markers;
use super::lagged_output_marker;
use super::observe_process_exit;
use super::omitted_output_marker;
use super::resolve_aggregated_output;
use super::split_valid_utf8_prefix_with_max;
use super::wait_for_process_output_drain;
use super::wait_for_process_output_for_result;

use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_execution::CommandExecutionLedger;
use crate::tools::command_execution::CompletionApplyResult;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use codex_protocol::protocol::ToolExecutionId;

#[tokio::test(start_paused = true)]
async fn trailing_output_waits_for_quiet_without_extending_the_hard_bound() {
    let start = tokio::time::Instant::now();
    let hard = start + super::TRAILING_OUTPUT_GRACE;
    let quiet = super::trailing_output_deadline(start, hard);
    assert!(quiet < hard);
    tokio::time::advance(Duration::from_millis(10)).await;
    let extended = super::trailing_output_deadline(tokio::time::Instant::now(), hard);
    assert!(extended > quiet);
    tokio::time::advance(Duration::from_millis(85)).await;
    assert_eq!(
        super::trailing_output_deadline(tokio::time::Instant::now(), hard),
        hard
    );
}

#[tokio::test]
async fn process_exit_before_async_watcher_registration_is_observed_once() {
    let ledger = CommandExecutionLedger::default();
    let command_execution_id = ledger.allocate_execution_id();
    let parent_tool_execution_id = ToolExecutionId("tool-execution-watcher".to_string());
    ledger
        .track_running_process_with_execution_id(
            command_execution_id,
            parent_tool_execution_id.clone(),
            73,
            CommandAttemptKey::new("exec_command", "local", "C:/repo", &["exit".to_string()]),
            RawOutputArtifact::unavailable("late watcher fixture"),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("track running process");
    let exit = CancellationToken::new();
    let output_drained = CancellationToken::new();
    exit.cancel();
    output_drained.cancel();

    assert_eq!(
        observe_process_exit(
            &exit,
            &ledger,
            73,
            command_execution_id,
            &parent_tool_execution_id,
            0,
        )
        .await,
        CompletionApplyResult::Applied
    );
    wait_for_process_output_drain(&output_drained).await;
    assert_eq!(
        ledger
            .retire_completed_process(command_execution_id, &parent_tool_execution_id)
            .await,
        CompletionApplyResult::Applied
    );
    assert_eq!(
        observe_process_exit(
            &exit,
            &ledger,
            73,
            command_execution_id,
            &parent_tool_execution_id,
            0,
        )
        .await,
        CompletionApplyResult::AlreadyApplied
    );
    assert!(ledger.running_process(73).await.is_none());
}

#[tokio::test]
async fn transcript_drain_does_not_overtake_raw_output_finalization() {
    for direct_runtime in [false, true] {
        for already_closed in [false, true] {
            let output_drained = CancellationToken::new();
            let output_closed = AtomicBool::new(already_closed);
            let output_closed_notify = Notify::new();
            let waiter = wait_for_process_output_for_result(
                direct_runtime,
                &output_drained,
                &output_closed,
                &output_closed_notify,
            );
            tokio::pin!(waiter);
            assert!(
                futures::poll!(&mut waiter).is_pending(),
                "both modes require transcript drain"
            );
            output_drained.cancel();
            if !direct_runtime && !already_closed {
                assert!(
                    futures::poll!(&mut waiter).is_pending(),
                    "artifact is still pending"
                );
                output_closed.store(true, Ordering::Release);
                output_closed_notify.notify_waiters();
            }
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("completion barrier should release");
            assert_eq!(
                output_closed.load(Ordering::Acquire),
                already_closed || !direct_runtime
            );
        }
    }
}

#[test]
fn split_valid_utf8_prefix_respects_max_bytes_for_ascii() {
    let mut buf = b"hello word!".as_slice();

    let first = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 5, /*flush_incomplete*/ false,
    )
    .expect("expected prefix");
    assert_eq!(first, b"hello".to_vec());
    assert_eq!(buf, b" word!".to_vec());

    let second = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 5, /*flush_incomplete*/ false,
    )
    .expect("expected prefix");
    assert_eq!(second, b" word".to_vec());
    assert_eq!(buf, b"!".to_vec());
}

#[test]
fn split_valid_utf8_prefix_avoids_splitting_utf8_codepoints() {
    // "é" is 2 bytes in UTF-8. With a max of 3 bytes, we should only emit 1 char (2 bytes).
    let mut buf = "ééé".as_bytes();

    let first = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 3, /*flush_incomplete*/ false,
    )
    .expect("expected prefix");
    assert_eq!(std::str::from_utf8(first).unwrap(), "é");
    assert_eq!(buf, "éé".as_bytes().to_vec());
}

#[test]
fn split_valid_utf8_prefix_makes_progress_on_invalid_utf8() {
    let mut buf: &[u8] = &[0xff, b'a', b'b'];

    let first = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 2, /*flush_incomplete*/ false,
    )
    .expect("expected prefix");
    assert_eq!(first, vec![0xff]);
    assert_eq!(buf, b"ab".to_vec());
}

#[test]
fn split_valid_utf8_prefix_waits_for_a_codepoint_split_across_chunks() {
    let mut bytes = vec![0xc3];
    let mut buf = bytes.as_slice();

    assert_eq!(
        split_valid_utf8_prefix_with_max(
            &mut buf, /*max_bytes*/ 8, /*flush_incomplete*/ false,
        ),
        None
    );
    assert_eq!(buf, vec![0xc3]);

    bytes.push(0xa9);
    buf = bytes.as_slice();

    let completed = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 8, /*flush_incomplete*/ false,
    )
    .expect("expected completed code point");
    assert_eq!(completed, "é".as_bytes());
    assert!(buf.is_empty());
}

#[test]
fn split_valid_utf8_prefix_flushes_permanently_incomplete_bytes_at_end_of_stream() {
    let mut buf: &[u8] = &[0xe2, 0x82];

    let first = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 8, /*flush_incomplete*/ true,
    )
    .expect("expected first incomplete byte");
    let second = split_valid_utf8_prefix_with_max(
        &mut buf, /*max_bytes*/ 8, /*flush_incomplete*/ true,
    )
    .expect("expected second incomplete byte");

    assert_eq!(first, vec![0xe2]);
    assert_eq!(second, vec![0x82]);
    assert!(buf.is_empty());
}

#[test]
fn lagged_output_is_explicit_in_the_transcript() {
    assert_eq!(
        String::from_utf8(lagged_output_marker(7)).expect("marker is UTF-8"),
        "\n[output unavailable: streaming receiver lagged by 7 chunk(s)]\n"
    );
}

#[test]
fn capacity_omission_is_distinct_from_broadcast_lag() {
    assert_eq!(
        String::from_utf8(omitted_output_marker(64)).expect("marker is UTF-8"),
        "\n[output truncated: 64 byte(s) omitted from the middle by the output retention limit]\n"
    );
}

#[test]
fn finalization_does_not_duplicate_existing_loss_markers() {
    let output = format!(
        "prefix{}{}",
        String::from_utf8(omitted_output_marker(64)).expect("omission marker"),
        String::from_utf8(lagged_output_marker(7)).expect("lag marker")
    );

    let finalized = append_output_loss_markers(output, 64, 7);

    assert_eq!(
        finalized
            .matches("64 byte(s) omitted from the middle")
            .count(),
        1
    );
    assert_eq!(
        finalized
            .matches("streaming receiver lagged by 7 chunk(s)")
            .count(),
        1
    );
}

#[tokio::test]
async fn final_loss_markers_survive_head_tail_eviction_without_duplication() {
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::new(16)));
    {
        let mut guard = transcript.lock().await;
        guard.push_chunk(&[b'a'; 16]);
        guard.record_lagged_chunks(7);
        guard.push_chunk(&[b'b'; 64]);
    }

    let aggregated = resolve_aggregated_output(&transcript, String::new()).await;

    assert_eq!(
        aggregated
            .matches("64 byte(s) omitted from the middle")
            .count(),
        1
    );
    assert_eq!(
        aggregated
            .matches("streaming receiver lagged by 7 chunk(s)")
            .count(),
        1
    );
    assert!(aggregated.contains("bbbbbbbb"));
}

#[tokio::test]
async fn final_capacity_marker_separates_nonadjacent_head_and_tail() {
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::new(8)));
    transcript.lock().await.push_chunk(b"pass---word");

    let aggregated = resolve_aggregated_output(&transcript, String::new()).await;

    assert_eq!(
        aggregated,
        format!(
            "pass{}word",
            String::from_utf8(omitted_output_marker(3)).expect("marker is UTF-8")
        )
    );
    assert!(!aggregated.contains("password"));
}

#[tokio::test]
async fn streaming_utf8_pending_bytes_are_isolated_between_stdout_and_stderr() {
    use crate::exec::OutputDeltaLimiter;
    use crate::unified_exec::process::ProcessOutputChunk;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ExecOutputStream;

    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let limiter = OutputDeltaLimiter::default();
    let mut pending = super::PendingOutput::default();
    // Each stream has its own split character: stdout is é and stderr is ñ.
    // Neither stream may complete or invalidate the other stream's prefix.
    for (stream, bytes) in [
        (ExecOutputStream::Stdout, vec![0xc3]),
        (ExecOutputStream::Stderr, vec![0xc3]),
        (ExecOutputStream::Stdout, vec![0xa9]),
        (ExecOutputStream::Stderr, vec![0xb1]),
    ] {
        super::process_chunk(
            &mut pending,
            &transcript,
            "stream-isolation",
            &session,
            &turn,
            &limiter,
            ProcessOutputChunk { stream, bytes },
        )
        .await;
    }
    super::flush_pending(
        &mut pending,
        &transcript,
        "stream-isolation",
        &session,
        &turn,
        &limiter,
    )
    .await;
    let mut deltas = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::ExecCommandOutputDelta(delta) = event.msg {
            assert_eq!(delta.call_id, "stream-isolation");
            deltas.push((delta.stream, delta.chunk));
        }
    }
    assert_eq!(
        deltas,
        vec![
            (ExecOutputStream::Stdout, "é".as_bytes().to_vec()),
            (ExecOutputStream::Stderr, "ñ".as_bytes().to_vec()),
        ]
    );
    assert!(pending.stdout.is_empty());
    assert!(pending.stderr.is_empty());
}

#[tokio::test]
async fn streaming_lag_flushes_both_utf8_prefixes_before_post_gap_bytes() {
    use crate::exec::OutputDeltaLimiter;
    use crate::unified_exec::process::ProcessOutputChunk;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ExecOutputStream;

    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let limiter = OutputDeltaLimiter::default();
    let mut pending = super::PendingOutput::default();
    for byte in [0xc3, 0xa9] {
        if byte == 0xa9 {
            // These bytes are not adjacent in either producer stream. A lag
            // must not let their UTF-8 projections fabricate an é character.
            super::handle_lagged_output(
                7,
                &mut pending,
                &transcript,
                "stream-gap",
                &session,
                &turn,
                &limiter,
            )
            .await;
        }
        for stream in [ExecOutputStream::Stdout, ExecOutputStream::Stderr] {
            super::process_chunk(
                &mut pending,
                &transcript,
                "stream-gap",
                &session,
                &turn,
                &limiter,
                ProcessOutputChunk {
                    stream,
                    bytes: vec![byte],
                },
            )
            .await;
        }
    }
    super::flush_pending(
        &mut pending,
        &transcript,
        "stream-gap",
        &session,
        &turn,
        &limiter,
    )
    .await;
    let mut deltas = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::ExecCommandOutputDelta(delta) = event.msg {
            assert_eq!(delta.call_id, "stream-gap");
            deltas.push((delta.stream, delta.chunk));
        }
    }
    assert_eq!(
        deltas,
        vec![
            (ExecOutputStream::Stdout, vec![0xc3]),
            (ExecOutputStream::Stderr, vec![0xc3]),
            (
                ExecOutputStream::Stdout,
                b"\n[output unavailable: streaming receiver lagged by 7 chunk(s)]\n".to_vec(),
            ),
            (ExecOutputStream::Stdout, vec![0xa9]),
            (ExecOutputStream::Stderr, vec![0xa9]),
        ]
    );
    assert_eq!(transcript.lock().await.lagged_chunks(), 7);
    assert!(pending.stdout.is_empty());
    assert!(pending.stderr.is_empty());
}

#[tokio::test]
async fn capped_live_output_preserves_transcript_without_growing_pending_buffers() {
    use crate::exec::EXEC_OUTPUT_DELTA_CAP_NOTICE;
    use crate::exec::MAX_EXEC_OUTPUT_DELTAS_PER_CALL;
    use crate::exec::OutputDeltaDecision;
    use crate::exec::OutputDeltaLimiter;
    use crate::unified_exec::process::ProcessOutputChunk;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ExecOutputStream;
    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let limiter = OutputDeltaLimiter::default();
    for _ in 0..MAX_EXEC_OUTPUT_DELTAS_PER_CALL - 1 {
        assert_eq!(limiter.claim(), OutputDeltaDecision::Emit);
    }
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let mut pending = super::PendingOutput::default();
    for bytes in [b"last live\n".to_vec(), b"cap trigger\n".to_vec()] {
        super::process_chunk(
            &mut pending,
            &transcript,
            "cap-test",
            &session,
            &turn,
            &limiter,
            ProcessOutputChunk {
                stream: ExecOutputStream::Stdout,
                bytes,
            },
        )
        .await;
    }
    assert!(limiter.is_suppressed());
    let capacities = (pending.stdout.capacity(), pending.stderr.capacity());
    let payload = vec![b'x'; 32_768];
    for stream in [ExecOutputStream::Stdout, ExecOutputStream::Stderr] {
        super::process_chunk(
            &mut pending,
            &transcript,
            "cap-test",
            &session,
            &turn,
            &limiter,
            ProcessOutputChunk {
                stream,
                bytes: payload.clone(),
            },
        )
        .await;
    }
    super::flush_pending(
        &mut pending,
        &transcript,
        "cap-test",
        &session,
        &turn,
        &limiter,
    )
    .await;
    assert!(pending.stdout.is_empty());
    assert!(pending.stderr.is_empty());
    assert_eq!(
        (pending.stdout.capacity(), pending.stderr.capacity()),
        capacities
    );
    let mut expected = b"last live\ncap trigger\n".to_vec();
    expected.extend_from_slice(&payload);
    expected.extend_from_slice(&payload);
    assert_eq!(
        resolve_aggregated_output(&transcript, String::new())
            .await
            .as_bytes(),
        expected
    );
    let mut chunks = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::ExecCommandOutputDelta(delta) = event.msg {
            assert_eq!(delta.call_id, "cap-test");
            chunks.push(delta.chunk);
        }
    }
    assert_eq!(
        chunks,
        vec![
            b"last live\n".to_vec(),
            EXEC_OUTPUT_DELTA_CAP_NOTICE.to_vec()
        ]
    );
}


#[tokio::test]
async fn completion_preserves_exit_capture_and_search_facts() -> anyhow::Result<()> {
    use codex_protocol::items::CommandExecutionStatus;
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::EventMsg;

    for case in 0..10 {
        let (session, turn, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let process = crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            None,
        )
        .await;
        let exit_code = match case {
            2 | 3 => None,
            4 | 6 | 7 => Some(1),
            5 => Some(2),
            _ => Some(0),
        };
        if case == 3 {
            process.terminate_confirmed().await?;
        } else {
            process.signal_exit_for_test(exit_code);
        }
        let observed_lifetime = process.observed_lifetime().expect("observed process exit");
        tokio::task::yield_now().await;
        process.signal_exit_for_test(exit_code);
        assert_eq!(process.observed_lifetime(), Some(observed_lifetime));
        let handles = process.output_handles();
        let drained = case != 6;
        handles.output_closed.store(drained, Ordering::Release);
        // Relay completion alone must not claim reader EOF.
        process.output_drained_token().cancel();
        if case == 7 {
            handles.stderr_buffer.lock().await.record_lagged_chunks(1);
        }
        if case == 8 {
            let mut output = handles.completion_output_buffer.lock().await;
            *output = HeadTailBuffer::new(2);
            output.push_chunk(b"omitted");
        }
        if case == 9 {
            handles.stdout_buffer.lock().await.push_chunk(&[0xff]);
        }
        let failure = (case == 1).then(|| "late harness failure".to_string());
        super::emit_exec_end_for_unified_exec(
            Arc::clone(&session),
            Arc::clone(&turn),
            "receipt".to_string(),
            vec!["rg".to_string(), "needle".to_string()],
            turn.cwd().clone().into(),
            "local".to_string(),
            None,
            Arc::new(Mutex::new(HeadTailBuffer::default())),
            String::new(),
            Some(&process),
            false,
            (4..=7).contains(&case),
            failure.clone(),
            false,
            // Deliberately unrelated collection/reporting interval. A public
            // completion must retain the first process-owned observation.
            Duration::from_secs(99),
            crate::tools::context::ToolCallSource::Direct,
            None,
            Some("original-execution"),
        )
        .await?;
        let mut canonical = None;
        let mut legacy = None;
        while let Ok(event) = events.try_recv() {
            match event.msg {
                EventMsg::ItemCompleted(event) => {
                    if let TurnItem::CommandExecution(item) = event.item {
                        canonical = Some(item);
                    }
                }
                EventMsg::ExecCommandEnd(event) => legacy = Some(event),
                _ => {}
            }
        }
        let item = canonical.expect("canonical completion");
        let legacy = legacy.expect("legacy completion");
        assert_eq!(legacy.duration, observed_lifetime);
        let metadata = item.output_metadata.as_ref().expect("capture facts");
        // This helper runs outside the original tool timing task-local scope.
        assert_eq!(item.execution_id.as_deref(), Some("original-execution"));
        assert_eq!(item.exit_code, exit_code, "case {case}");
        assert_eq!(legacy.exit_code, exit_code, "case {case}");
        assert_eq!(legacy.output_metadata.as_ref(), Some(metadata));
        assert!(metadata.process_exited);
        assert_eq!(metadata.output_drained, drained);
        assert_eq!(metadata.failure_cause, failure);
        assert_eq!(metadata.search_no_match, case == 4);
        assert_eq!(metadata.aggregated_output_is_exact, case != 8);
        assert_eq!(metadata.streams_are_exact, case != 7);
        assert_eq!(metadata.decoding_lossy, case == 9);
        assert_eq!(
            item.status,
            if matches!(case, 0 | 4 | 8 | 9) {
                CommandExecutionStatus::Completed
            } else {
                CommandExecutionStatus::Failed
            },
            "case {case}",
        );
    }
    Ok(())
}
