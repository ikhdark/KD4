use super::*;

use base64::Engine;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_sandboxing::SandboxType;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context as TaskContext;
use std::task::Poll;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;
use tokio::time::timeout;

#[test]
fn expiration_timeout_milliseconds_preserve_duration_and_saturate_overflow() {
    for (duration, expected_ms) in [
        (Duration::ZERO, 0),
        (Duration::from_millis(1234), 1234),
        (Duration::from_millis(u64::MAX), u64::MAX),
        (
            Duration::from_millis(u64::MAX) + Duration::from_millis(1),
            u64::MAX,
        ),
    ] {
        let expiration = ExecExpiration::Timeout(duration);
        assert_eq!(expiration.timeout_ms(), Some(expected_ms));
        let expiration = expiration.with_cancellation(CancellationToken::new());
        assert_eq!(expiration.timeout_ms(), Some(expected_ms));
        let expiration = expiration.with_cancellation(CancellationToken::new());
        assert_eq!(expiration.timeout_ms(), Some(expected_ms));
    }

    assert_eq!(
        ExecExpiration::Cancellation(CancellationToken::new()).timeout_ms(),
        None
    );
}

struct ChunkedReader {
    chunks: VecDeque<Vec<u8>>,
}

struct PrefixThenPendingReader {
    prefix: Option<Vec<u8>>,
    prefix_read: Arc<tokio::sync::Notify>,
}

impl AsyncRead for PrefixThenPendingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(prefix) = self.prefix.take() {
            buf.put_slice(&prefix);
            self.prefix_read.notify_one();
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn byte_stream_output(text: impl Into<Vec<u8>>) -> StreamOutput<Vec<u8>> {
    StreamOutput {
        text: text.into(),
        truncated_after_lines: None,
        truncated: false,
    }
}

#[test]
fn output_capture_preserves_observed_order_and_requires_excess_for_truncation() {
    let mut capture = OutputCapture::new(Some(6));
    capture.append(b"a");
    capture.append(b"BC");
    capture.append(b"def");
    let exact = capture.snapshot();
    assert_eq!(exact.text, b"aBCdef");
    assert!(!exact.truncated);

    capture.append(b"g");
    let overflowed = capture.snapshot();
    assert_eq!(overflowed.text, b"aBCefg");
    assert!(overflowed.truncated);

    capture.append(b"0123456789");
    assert_eq!(capture.snapshot().text, b"aBC789");
}

impl ChunkedReader {
    fn new(chunks: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            chunks: chunks.into_iter().collect(),
        }
    }
}

impl AsyncRead for ChunkedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(chunk) = self.chunks.pop_front() else {
            return Poll::Ready(Ok(()));
        };
        assert!(chunk.len() <= buf.remaining());
        buf.put_slice(&chunk);
        Poll::Ready(Ok(()))
    }
}

fn streamed_output_chunks(receiver: &async_channel::Receiver<Event>) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected exec command output delta");
        };
        assert_eq!(event.id, "sub");
        assert_eq!(delta.call_id, "call");
        assert_eq!(delta.stream, ExecOutputStream::Stdout);
        chunks.push(delta.chunk);
    }
    chunks
}

#[test]
fn temporary_output_channel_backpressure_does_not_disable_later_live_output() {
    let (tx, rx) = async_channel::bounded(1);
    let stream = StdoutStream::without_progress("sub".into(), "call".into(), tx);
    let limiter = OutputDeltaLimiter::default();

    assert_eq!(
        try_send_limited_output_delta(&stream, false, b"first".to_vec(), &limiter),
        OutputDeltaSendOutcome::Continue
    );
    assert_eq!(
        try_send_limited_output_delta(&stream, false, b"dropped".to_vec(), &limiter),
        OutputDeltaSendOutcome::Continue
    );
    assert_eq!(streamed_output_chunks(&rx), vec![b"first".to_vec()]);

    assert_eq!(
        try_send_limited_output_delta(&stream, false, b"later".to_vec(), &limiter),
        OutputDeltaSendOutcome::Continue
    );
    assert_eq!(streamed_output_chunks(&rx), vec![b"later".to_vec()]);
}

#[test]
fn a_closed_output_channel_ends_live_output() {
    // A dropped receiver is permanent, unlike momentary backpressure, so the
    // caller must stop attempting live deltas for the rest of the command.
    let (tx, rx) = async_channel::bounded(1);
    let stream = StdoutStream::without_progress("sub".into(), "call".into(), tx);
    let limiter = OutputDeltaLimiter::default();
    drop(rx);

    assert_eq!(
        try_send_limited_output_delta(&stream, false, b"undeliverable".to_vec(), &limiter),
        OutputDeltaSendOutcome::Stop
    );
}

fn make_exec_output(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    aggregated: &str,
) -> ExecToolCallOutput {
    ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(stdout.to_string()),
        stderr: StreamOutput::new(stderr.to_string()),
        aggregated_output: StreamOutput::new(aggregated.to_string()),
        duration: Duration::from_millis(1),
        timed_out: false,
    }
}

#[test]
fn sandbox_detection_requires_keywords() {
    let output = make_exec_output(/*exit_code*/ 1, "", "", "");
    assert!(!is_likely_sandbox_denied(
        SandboxType::WindowsRestrictedToken,
        &output
    ));
}

#[test]
fn sandbox_detection_identifies_keyword_in_stderr() {
    let output = make_exec_output(/*exit_code*/ 1, "", "Operation not permitted", "");
    assert!(is_likely_sandbox_denied(
        SandboxType::WindowsRestrictedToken,
        &output
    ));
}

#[test]
fn sandbox_detection_respects_quick_reject_exit_codes() {
    let output = make_exec_output(/*exit_code*/ 127, "", "command not found", "");
    assert!(!is_likely_sandbox_denied(
        SandboxType::WindowsRestrictedToken,
        &output
    ));
}

#[test]
fn sandbox_detection_ignores_non_sandbox_mode() {
    let output = make_exec_output(/*exit_code*/ 1, "", "Operation not permitted", "");
    assert!(!is_likely_sandbox_denied(SandboxType::None, &output));
}

#[test]
fn sandbox_detection_ignores_network_policy_text_in_non_sandbox_mode() {
    let output = make_exec_output(
        /*exit_code*/ 0,
        "",
        "",
        r#"CODEX_NETWORK_POLICY_DECISION {"decision":"ask","reason":"not_allowed","source":"decider","protocol":"http","host":"google.com","port":80}"#,
    );
    assert!(!is_likely_sandbox_denied(SandboxType::None, &output));
}

#[test]
fn sandbox_detection_uses_aggregated_output() {
    let output = make_exec_output(
        /*exit_code*/ 101,
        "",
        "",
        "cargo failed: Read-only file system when writing target",
    );
    assert!(is_likely_sandbox_denied(
        SandboxType::WindowsRestrictedToken,
        &output
    ));
}

#[test]
fn sandbox_detection_ignores_application_sandbox_text() {
    let output = make_exec_output(
        /*exit_code*/ 1,
        "",
        "test failed: sandbox fixture returned the wrong value",
        "",
    );
    assert!(!is_likely_sandbox_denied(
        SandboxType::WindowsRestrictedToken,
        &output
    ));
}

#[test]
fn sandbox_detection_ignores_generic_write_failures() {
    let output = make_exec_output(
        /*exit_code*/ 1,
        "",
        "failed to write file: no space left on device",
        "",
    );
    assert!(!is_likely_sandbox_denied(
        SandboxType::WindowsRestrictedToken,
        &output
    ));
}

#[test]
fn sandbox_detection_ignores_network_policy_text_with_zero_exit_code() {
    let output = make_exec_output(
        /*exit_code*/ 0,
        "",
        "",
        r#"CODEX_NETWORK_POLICY_DECISION {"decision":"ask","source":"decider","protocol":"http","host":"google.com","port":80}"#,
    );

    assert!(!is_likely_sandbox_denied(
        SandboxType::WindowsRestrictedToken,
        &output
    ));
}

#[tokio::test]
async fn read_output_limits_retained_bytes_for_shell_capture() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let mut bytes = vec![b'a'; EXEC_OUTPUT_MAX_BYTES.saturating_add(128 * 1024)];
    bytes[..5].copy_from_slice(b"HEAD\n");
    bytes.extend_from_slice(b"\nerror: could not compile\nTAIL\n");
    tokio::spawn(async move {
        writer.write_all(&bytes).await.expect("write");
    });

    let out = read_output(
        reader,
        /*stream*/ None,
        /*is_stderr*/ false,
        Some(EXEC_OUTPUT_MAX_BYTES),
        Arc::new(OutputDeltaLimiter::default()),
    )
    .await
    .expect("read");
    assert_eq!(out.text.len(), EXEC_OUTPUT_MAX_BYTES);
    assert!(out.truncated);
    assert!(out.text.starts_with(b"HEAD\n"));
    assert!(out.text.ends_with(b"\nerror: could not compile\nTAIL\n"));
    assert!(
        String::from_utf8(out.text)
            .expect("ASCII output")
            .contains("[... output truncated ...]")
    );
}

#[tokio::test]
async fn read_output_notifies_the_command_progress_watchdog() {
    let progress = CommandProgress::new();
    let observer = progress.subscribe();
    let (tx_event, _rx_event) = async_channel::unbounded();

    read_output(
        ChunkedReader::new([b"progress".to_vec()]),
        Some(StdoutStream {
            sub_id: "sub".to_string(),
            call_id: "call".to_string(),
            tx_event,
            progress: Some(progress.clone()),
        }),
        /*is_stderr*/ false,
        /*max_bytes*/ None,
        Arc::new(OutputDeltaLimiter::default()),
    )
    .await
    .expect("read output");

    assert!(
        observer
            .has_changed()
            .expect("progress channel remains open")
    );
}

#[tokio::test]
async fn read_output_drains_and_retains_pipe_when_live_event_queue_is_full() {
    let (tx_event, rx_event) = async_channel::bounded(1);
    let stream = StdoutStream {
        sub_id: "sub".to_string(),
        call_id: "call".to_string(),
        tx_event,
        progress: None,
    };
    stream
        .tx_event
        .try_send(output_delta_event(
            &stream,
            /*is_stderr*/ false,
            b"prefill".to_vec(),
        ))
        .expect("prefill bounded live event queue");

    let expected = vec![b'x'; READ_CHUNK_SIZE * 4];
    let writer_expected = expected.clone();
    let (mut writer, reader) = tokio::io::duplex(128);
    let writer_task = tokio::spawn(async move {
        writer
            .write_all(&writer_expected)
            .await
            .expect("write all pipe output");
    });

    let output = timeout(
        Duration::from_secs(1),
        read_output(
            reader,
            Some(stream),
            /*is_stderr*/ false,
            /*max_bytes*/ None,
            Arc::new(OutputDeltaLimiter::default()),
        ),
    )
    .await
    .expect("a full live event queue must not block pipe drain")
    .expect("read output");
    writer_task.await.expect("writer task joins");

    assert_eq!(output.text, expected);
    assert_eq!(
        rx_event.len(),
        1,
        "full queue should remain best-effort only"
    );
}

#[tokio::test]
async fn read_output_preserves_utf8_codepoints_split_across_reads() {
    let expected = "A€𐍈Z";
    let reader = ChunkedReader::new([
        vec![b'A', 0xe2],
        vec![0x82],
        vec![0xac, 0xf0, 0x90],
        vec![0x8d],
        vec![0x88, b'Z'],
    ]);
    let (tx_event, rx_event) = async_channel::unbounded();

    let output = read_output(
        reader,
        Some(StdoutStream {
            sub_id: "sub".to_string(),
            call_id: "call".to_string(),
            tx_event,
            progress: None,
        }),
        /*is_stderr*/ false,
        /*max_bytes*/ None,
        Arc::new(OutputDeltaLimiter::default()),
    )
    .await
    .expect("read output");

    assert_eq!(output.text, expected.as_bytes());
    let chunks = streamed_output_chunks(&rx_event);
    assert!(
        chunks
            .iter()
            .all(|chunk| std::str::from_utf8(chunk).is_ok())
    );
    assert_eq!(chunks.concat(), expected.as_bytes());
    let rendered = chunks
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk))
        .collect::<String>();
    assert_eq!(rendered, expected);
}

#[tokio::test]
async fn read_output_makes_progress_when_pending_exceeds_read_chunk_size() {
    let mut first = vec![b'a'; READ_CHUNK_SIZE];
    first[READ_CHUNK_SIZE - 1] = 0xf0;
    let mut second = vec![b'b'; READ_CHUNK_SIZE];
    second[..3].copy_from_slice(&[0x90, 0x8d, 0x88]);
    let expected = [first.as_slice(), second.as_slice()].concat();
    let reader = ChunkedReader::new([first, second]);
    let (tx_event, rx_event) = async_channel::unbounded();

    let output = read_output(
        reader,
        Some(StdoutStream {
            sub_id: "sub".to_string(),
            call_id: "call".to_string(),
            tx_event,
            progress: None,
        }),
        /*is_stderr*/ false,
        /*max_bytes*/ None,
        Arc::new(OutputDeltaLimiter::default()),
    )
    .await
    .expect("read output");

    assert_eq!(output.text, expected);
    let chunks = streamed_output_chunks(&rx_event);
    assert!(
        chunks
            .iter()
            .all(|chunk| std::str::from_utf8(chunk).is_ok())
    );
    assert_eq!(chunks.concat(), expected);
}

#[tokio::test]
async fn read_output_flushes_terminal_incomplete_utf8_once() {
    let reader = ChunkedReader::new([vec![b'x', 0xe2], vec![0x82]]);
    let (tx_event, rx_event) = async_channel::unbounded();

    let output = read_output(
        reader,
        Some(StdoutStream {
            sub_id: "sub".to_string(),
            call_id: "call".to_string(),
            tx_event,
            progress: None,
        }),
        /*is_stderr*/ false,
        /*max_bytes*/ None,
        Arc::new(OutputDeltaLimiter::default()),
    )
    .await
    .expect("read output");

    assert_eq!(output.text, vec![b'x', 0xe2, 0x82]);
    let chunks = streamed_output_chunks(&rx_event);
    assert_eq!(chunks, vec![b"x".to_vec(), vec![0xe2, 0x82]]);
    let rendered = chunks
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk))
        .collect::<String>();
    assert_eq!(rendered, "x�");
}

#[tokio::test]
async fn read_output_retains_all_bytes_for_full_buffer_capture() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let bytes = vec![b'a'; EXEC_OUTPUT_MAX_BYTES.saturating_add(128 * 1024)];
    let expected = bytes.clone();
    // The duplex pipe is smaller than `bytes`, so the writer must run concurrently
    // with `read_output()` or `write_all()` will block once the buffer fills up.
    tokio::spawn(async move {
        writer.write_all(&bytes).await.expect("write");
    });

    let out = read_output(
        reader,
        /*stream*/ None,
        /*is_stderr*/ false,
        /*max_bytes*/ None,
        Arc::new(OutputDeltaLimiter::default()),
    )
    .await
    .expect("read");
    assert_eq!(out.text, expected);
    assert!(!out.truncated);
}

#[tokio::test]
async fn aggregate_capture_retains_observed_head_and_tail_from_both_streams() {
    let marker = "\n[... output truncated ...]\n";
    for (stdout, stderr, cap, expected, truncated) in [
        (
            "a".repeat(128),
            "b".repeat(128),
            Some(128),
            format!("{}{marker}{}", "a".repeat(36), "b".repeat(64)),
            true,
        ),
        (
            "a".repeat(10),
            "b".repeat(128),
            Some(128),
            format!(
                "{}{}{marker}{}",
                "a".repeat(10),
                "b".repeat(26),
                "b".repeat(64)
            ),
            true,
        ),
        (
            "a".repeat(128),
            "b".to_string(),
            Some(128),
            format!("{}{marker}{}b", "a".repeat(36), "a".repeat(63)),
            true,
        ),
        (
            "aaaa".to_string(),
            "bbb".to_string(),
            Some(128),
            "aaaabbb".to_string(),
            false,
        ),
        (
            "a".repeat(128),
            "b".repeat(128),
            None,
            format!("{}{}", "a".repeat(128), "b".repeat(128)),
            false,
        ),
    ] {
        let aggregate = Arc::new(Mutex::new(OutputCapture::new(cap)));
        for (bytes, is_stderr) in [(stdout.into_bytes(), false), (stderr.into_bytes(), true)] {
            let capture = Arc::new(Mutex::new(OutputCapture::new(cap)));
            read_output_into_capture(
                ChunkedReader::new([bytes]),
                None,
                is_stderr,
                capture,
                Some(Arc::clone(&aggregate)),
                Arc::new(OutputDeltaLimiter::default()),
            )
            .await
            .expect("read production output path");
        }
        let output = aggregate.lock().expect("capture lock").snapshot();
        assert_eq!(output.text, expected.as_bytes());
        assert_eq!(output.truncated, truncated);
    }
}

#[tokio::test]
async fn retained_output_truncation_preserves_utf8_at_both_cuts() {
    let input = format!("{}END", "é🙂".repeat(30));
    for (cap, expected, truncated) in [
        (
            127,
            format!(
                "{}é\n[... output truncated ...]\n{}END",
                "é🙂".repeat(5),
                "é🙂".repeat(10)
            ),
            true,
        ),
        (input.len(), input.clone(), false),
    ] {
        let capture = Arc::new(Mutex::new(OutputCapture::new(Some(cap))));
        let aggregate = Arc::new(Mutex::new(OutputCapture::new(Some(cap))));
        read_output_into_capture(
            ChunkedReader::new(input.as_bytes().chunks(3).map(<[u8]>::to_vec)),
            None,
            false,
            Arc::clone(&capture),
            Some(Arc::clone(&aggregate)),
            Arc::new(OutputDeltaLimiter::default()),
        )
        .await
        .expect("read production output path");
        for capture in [capture, aggregate] {
            let output = capture.lock().expect("capture lock").snapshot();
            assert!(output.text.len() <= cap);
            assert_eq!(
                String::from_utf8(output.text).expect("valid UTF-8"),
                expected
            );
            assert_eq!(output.truncated, truncated);
        }
    }
}

#[test]
fn full_buffer_capture_policy_disables_only_caps() {
    assert_eq!(ExecCapturePolicy::FullBuffer.retained_bytes_cap(), None);
    assert_eq!(
        ExecCapturePolicy::FullBuffer.io_drain_timeout(),
        Duration::from_millis(IO_DRAIN_TIMEOUT_MS)
    );
}

#[tokio::test]
async fn combined_exec_cancellation_waits_inline_for_every_source() {
    let first = CancellationToken::new();
    let second = CancellationToken::new();
    let third = CancellationToken::new();
    let expiration = ExecExpiration::Cancellation(first)
        .with_cancellation(second)
        .with_cancellation(third.clone());

    let ExecExpiration::CancellationSet(cancellations) = &expiration else {
        panic!("combined cancellation should retain its sources without a relay task");
    };
    assert_eq!(cancellations.len(), 3);

    third.cancel();
    assert_eq!(
        expiration.wait_with_outcome().await,
        ExecExpirationOutcome::Cancelled
    );
}

#[tokio::test]
async fn exec_full_buffer_capture_honors_expiration() -> Result<()> {
    let command = vec![
        "powershell.exe".to_string(),
        "-NonInteractive".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Milliseconds 50; [Console]::Out.Write('hello')".to_string(),
    ];

    let env: HashMap<String, String> = std::env::vars().collect();
    let output = exec(
        ExecParams {
            command,
            codex_home: codex_utils_absolute_path::AbsolutePathBuf::current_dir()?,
            cwd: codex_utils_absolute_path::AbsolutePathBuf::current_dir()?,
            expiration: 1.into(),
            capture_policy: ExecCapturePolicy::FullBuffer,
            env,
            network: None,
            network_environment_id: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            windows_sandbox_level: WindowsSandboxLevel::Disabled,
            windows_sandbox_private_desktop: false,
            justification: None,
            arg0: None,
        },
        NetworkSandboxPolicy::Enabled,
        /*stdout_stream*/ None,
        /*after_spawn*/ None,
    )
    .await?;

    assert!(output.timed_out);
    assert!(!output.stdout.from_utf8_lossy().text.contains("hello"));

    Ok(())
}

#[tokio::test]
async fn windows_direct_exec_completes_for_trivial_command() -> Result<()> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        exec(
            ExecParams {
                command: vec![
                    "cmd.exe".to_string(),
                    "/d".to_string(),
                    "/c".to_string(),
                    "exit 0".to_string(),
                ],
                codex_home: codex_utils_absolute_path::AbsolutePathBuf::current_dir()?,
                cwd: codex_utils_absolute_path::AbsolutePathBuf::current_dir()?,
                expiration: ExecExpiration::Timeout(Duration::from_secs(5)),
                capture_policy: ExecCapturePolicy::FullBuffer,
                env,
                network: None,
                network_environment_id: None,
                sandbox_permissions: SandboxPermissions::UseDefault,
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                windows_sandbox_private_desktop: false,
                justification: None,
                arg0: None,
            },
            NetworkSandboxPolicy::Enabled,
            /*stdout_stream*/ None,
            /*after_spawn*/ None,
        ),
    )
    .await
    .expect("trivial Windows command should not hang")?;

    assert_eq!(output.exit_status.code(), Some(0));
    assert!(!output.timed_out);
    Ok(())
}

#[tokio::test]
async fn forced_direct_exec_termination_reaps_the_child() -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir()?;
    let managed_root = ManagedRootProcess::reserve_with_reclaim().await?;
    let child = spawn_child_async(SpawnChildRequest {
        program: PathBuf::from("powershell.exe"),
        args: vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 30".to_string(),
        ],
        arg0: None,
        cwd,
        network_sandbox_policy: NetworkSandboxPolicy::Enabled,
        network: None,
        stdio_policy: StdioPolicy::RedirectForShellTool,
        env: std::env::vars().collect(),
        creation_flags: 0,
    })
    .await?;
    managed_root.attach_and_resume(child.id().expect("child process id"))?;
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, child.id().expect("child id")) };
    assert!(
        !raw.is_null(),
        "observe child: {}",
        io::Error::last_os_error()
    );
    let observed = unsafe { OwnedHandle::from_raw_handle(raw) };

    terminate_and_reap_child_process_tree(child, managed_root).await?;

    assert_eq!(
        unsafe { WaitForSingleObject(observed.as_raw_handle(), 0) },
        WAIT_OBJECT_0,
        "owned termination must finish after actual child exit"
    );
    Ok(())
}

#[tokio::test]
async fn output_drain_readers_complete_normally_before_shared_deadline() -> Result<()> {
    let stdout = tokio::spawn(async { Ok::<_, io::Error>(byte_stream_output(b"stdout")) });
    let stderr = tokio::spawn(async { Ok::<_, io::Error>(byte_stream_output(b"stderr")) });

    let (stdout, stderr) = await_output_until_deadline(
        stdout,
        stderr,
        tokio::time::Instant::now() + Duration::from_secs(1),
    )
    .await?;

    assert_eq!(stdout.text, b"stdout");
    assert_eq!(stderr.text, b"stderr");
    Ok(())
}

#[tokio::test]
async fn output_drain_preserves_completed_reader_and_aborts_unfinished_reader() -> Result<()> {
    let aborted = Arc::new(AtomicBool::new(false));
    let stdout = tokio::spawn(async { Ok::<_, io::Error>(byte_stream_output(b"stdout")) });
    let stderr = tokio::spawn({
        let aborted = Arc::clone(&aborted);
        async move {
            let _drop_flag = DropFlag(aborted);
            std::future::pending::<io::Result<StreamOutput<Vec<u8>>>>().await
        }
    });

    let (stdout, stderr) = await_output_until_deadline(
        stdout,
        stderr,
        tokio::time::Instant::now() + Duration::from_millis(100),
    )
    .await?;
    tokio::task::yield_now().await;

    assert_eq!(stdout.text, b"stdout");
    assert!(stderr.text.is_empty());
    assert!(aborted.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test]
async fn output_drain_readers_share_one_deadline_window() -> Result<()> {
    let stdout =
        tokio::spawn(async { std::future::pending::<io::Result<StreamOutput<Vec<u8>>>>().await });
    let stderr =
        tokio::spawn(async { std::future::pending::<io::Result<StreamOutput<Vec<u8>>>>().await });
    let started_at = tokio::time::Instant::now();

    let (stdout, stderr) =
        await_output_until_deadline(stdout, stderr, started_at + Duration::from_millis(500))
            .await?;

    assert!(stdout.text.is_empty());
    assert!(stderr.text.is_empty());
    assert!(started_at.elapsed() < Duration::from_millis(800));
    Ok(())
}

#[tokio::test]
async fn output_drain_timeout_preserves_captured_prefix_and_marks_it_truncated() -> Result<()> {
    let prefix_read = Arc::new(tokio::sync::Notify::new());
    let stdout_capture = Arc::new(Mutex::new(OutputCapture::new(None)));
    let stderr_capture = Arc::new(Mutex::new(OutputCapture::new(None)));
    let aggregate_capture = Arc::new(Mutex::new(OutputCapture::new(None)));
    let stdout = tokio::spawn(read_output_into_capture(
        PrefixThenPendingReader {
            prefix: Some(b"retained prefix".to_vec()),
            prefix_read: Arc::clone(&prefix_read),
        },
        None,
        false,
        Arc::clone(&stdout_capture),
        Some(Arc::clone(&aggregate_capture)),
        Arc::new(OutputDeltaLimiter::default()),
    ));
    prefix_read.notified().await;
    let stderr = tokio::spawn({
        let stderr_capture = Arc::clone(&stderr_capture);
        let aggregate_capture = Arc::clone(&aggregate_capture);
        async move {
            aggregate_capture.lock().unwrap().append(b"stderr");
            stderr_capture.lock().unwrap().append(b"stderr");
            Ok::<_, io::Error>(())
        }
    });

    let (stdout, stderr) = await_captured_output_until_deadline(
        stdout,
        stderr,
        stdout_capture,
        stderr_capture,
        Arc::clone(&aggregate_capture),
        tokio::time::Instant::now() + Duration::from_millis(100),
    )
    .await?;

    let drain_notice = b"\n[... output capture stopped: pipe drain deadline exceeded ...]\n";
    assert_eq!(
        stdout.text,
        [b"retained prefix".as_slice(), drain_notice].concat()
    );
    assert!(stdout.truncated);
    assert_eq!(stderr.text, b"stderr");
    assert!(!stderr.truncated);
    let aggregated = aggregate_capture.lock().unwrap().snapshot();
    assert_eq!(
        aggregated.text,
        [b"retained prefixstderr".as_slice(), drain_notice].concat()
    );
    assert!(aggregated.truncated);
    Ok(())
}

#[tokio::test]
async fn output_drain_simultaneous_failures_return_stdout_error_first() {
    let stdout =
        tokio::spawn(async { Err::<StreamOutput<Vec<u8>>, _>(io::Error::other("stdout failure")) });
    let stderr =
        tokio::spawn(async { Err::<StreamOutput<Vec<u8>>, _>(io::Error::other("stderr failure")) });

    let error = await_output_until_deadline(
        stdout,
        stderr,
        tokio::time::Instant::now() + Duration::from_secs(1),
    )
    .await
    .expect_err("both output readers should fail");

    assert_eq!(error.to_string(), "stdout failure");
}

#[tokio::test]
async fn output_drain_stdout_join_error_precedes_stderr_io_error() {
    let stdout =
        tokio::spawn(async { std::future::pending::<io::Result<StreamOutput<Vec<u8>>>>().await });
    stdout.abort();
    let stderr =
        tokio::spawn(async { Err::<StreamOutput<Vec<u8>>, _>(io::Error::other("stderr failure")) });

    let error = await_output_until_deadline(
        stdout,
        stderr,
        tokio::time::Instant::now() + Duration::from_secs(1),
    )
    .await
    .expect_err("both output readers should fail");

    assert!(error.to_string().contains("cancelled"));
}

#[tokio::test]
async fn process_exec_tool_call_preserves_full_buffer_capture_policy() -> Result<()> {
    let byte_count = EXEC_OUTPUT_MAX_BYTES.saturating_add(128 * 1024);

    let command = vec![
        "powershell.exe".to_string(),
        "-NonInteractive".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        format!("Start-Sleep -Milliseconds 50; [Console]::Out.Write('a' * {byte_count})"),
    ];

    let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir()?;
    let permission_profile = PermissionProfile::Disabled;
    let output = process_exec_tool_call(
        ExecParams {
            command,
            codex_home: cwd.clone(),
            cwd: cwd.clone(),
            expiration: 30_000.into(),
            capture_policy: ExecCapturePolicy::FullBuffer,
            env: std::env::vars().collect(),
            network: None,
            network_environment_id: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            windows_sandbox_level: WindowsSandboxLevel::Disabled,
            windows_sandbox_private_desktop: false,
            justification: None,
            arg0: None,
        },
        &permission_profile,
        &cwd,
        std::slice::from_ref(&cwd),
        /*stdout_stream*/ None,
    )
    .await?;

    assert!(!output.timed_out);
    assert_eq!(output.stdout.text.len(), byte_count);

    Ok(())
}

#[test]
fn windows_restricted_token_skips_external_sandbox_policies() {
    let permission_profile = PermissionProfile::External {
        network: NetworkSandboxPolicy::Restricted,
    };

    assert!(!permission_profile_supports_windows_restricted_token_sandbox(&permission_profile));
}

#[test]
fn windows_restricted_token_supports_read_only_profiles() {
    let permission_profile = PermissionProfile::read_only();

    assert!(permission_profile_supports_windows_restricted_token_sandbox(&permission_profile));
}

#[test]
fn windows_proxy_enforcement_uses_elevated_backend() {
    assert!(!windows_sandbox_uses_elevated_backend(
        WindowsSandboxLevel::RestrictedToken,
        /*proxy_enforced*/ false,
    ));
    assert!(windows_sandbox_uses_elevated_backend(
        WindowsSandboxLevel::RestrictedToken,
        /*proxy_enforced*/ true,
    ));
    assert!(windows_sandbox_uses_elevated_backend(
        WindowsSandboxLevel::Elevated,
        /*proxy_enforced*/ false,
    ));
}

#[test]
fn windows_restricted_token_rejects_network_only_restrictions() {
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::unrestricted(),
        NetworkSandboxPolicy::Restricted,
    );
    let sandbox_policy_cwd = AbsolutePathBuf::current_dir().expect("cwd");

    assert_eq!(
            unsupported_windows_restricted_token_sandbox_reason(
                SandboxType::WindowsRestrictedToken,
                &permission_profile,
                &sandbox_policy_cwd,
                WindowsSandboxLevel::RestrictedToken,
            ),
            Some(
                "windows sandbox backend cannot enforce file_system=Unrestricted, network=Restricted, permission_profile=Managed; refusing to run unsandboxed".to_string()
            )
        );
}

#[test]
fn windows_restricted_token_rejects_managed_root_write_profiles() {
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::Root,
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );
    let sandbox_policy_cwd = AbsolutePathBuf::current_dir().expect("cwd");

    assert_eq!(
        unsupported_windows_restricted_token_sandbox_reason(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &sandbox_policy_cwd,
            WindowsSandboxLevel::RestrictedToken,
        ),
        Some(
            "windows sandbox backend cannot enforce file_system=Restricted, network=Restricted, permission_profile=Managed; refusing to run unsandboxed"
                .to_string()
        )
    );
}

#[test]
fn windows_restricted_token_allows_read_only_profiles() {
    let permission_profile = PermissionProfile::read_only();
    let sandbox_policy_cwd = AbsolutePathBuf::current_dir().expect("cwd");

    assert_eq!(
        unsupported_windows_restricted_token_sandbox_reason(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &sandbox_policy_cwd,
            WindowsSandboxLevel::RestrictedToken,
        ),
        None
    );
}

#[test]
fn windows_restricted_token_allows_workspace_write_profiles() {
    let permission_profile = PermissionProfile::workspace_write_with(
        &[],
        NetworkSandboxPolicy::Restricted,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    );
    let sandbox_policy_cwd = AbsolutePathBuf::current_dir().expect("cwd");

    assert_eq!(
        unsupported_windows_restricted_token_sandbox_reason(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &sandbox_policy_cwd,
            WindowsSandboxLevel::RestrictedToken,
        ),
        None
    );
}

#[test]
fn windows_elevated_allows_split_restricted_read_policies() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let docs = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
        temp_dir.path().join("docs"),
    )
    .expect("absolute docs");
    std::fs::create_dir_all(docs.as_path()).expect("create docs");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path { path: docs },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        unsupported_windows_restricted_token_sandbox_reason(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &temp_dir.path().abs(),
            WindowsSandboxLevel::Elevated,
        ),
        None
    );
}

#[test]
fn windows_restricted_token_rejects_split_only_filesystem_policies() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let docs = temp_dir.path().join("docs");
    std::fs::create_dir_all(&docs).expect("create docs");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::project_roots(
                    /*subpath*/ None,
                ),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path {
                path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&docs)
                    .expect("absolute docs"),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        unsupported_windows_restricted_token_sandbox_reason(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &temp_dir.path().abs(),
            WindowsSandboxLevel::RestrictedToken,
        ),
        Some(
            "windows unelevated restricted-token sandbox cannot enforce split filesystem read restrictions directly; refusing to run unsandboxed"
                .to_string()
        )
    );
}

#[test]
fn windows_restricted_token_supports_root_write_read_only_carveouts() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let docs = temp_dir.path().join("docs");
    std::fs::create_dir_all(&docs).expect("create docs");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::Root,
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path {
                path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&docs)
                    .expect("absolute docs"),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    let docs = dunce::canonicalize(&docs).expect("canonical docs").abs();
    assert_eq!(
        resolve_windows_restricted_token_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &temp_dir.path().abs(),
            WindowsSandboxLevel::RestrictedToken,
        ),
        Ok(Some(WindowsSandboxFilesystemOverrides {
            read_roots_override: None,
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            additional_deny_read_paths: vec![],
            additional_deny_write_paths: vec![docs],
        }))
    );
}

#[test]
fn windows_restricted_token_supports_full_read_split_write_read_carveouts() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let cwd = dunce::canonicalize(temp_dir.path())
        .expect("canonicalize temp dir")
        .abs();
    let docs = cwd.join("docs");
    std::fs::create_dir_all(docs.as_path()).expect("create docs");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::Root,
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::project_roots(
                    /*subpath*/ None,
                ),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path { path: docs.clone() },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    // The workspace-write compatibility projection already protects top-level
    // `.codex`, so the restricted-token overlay only needs the extra read-only
    // docs carveout.
    let expected_deny_write_paths = vec![docs];

    assert_eq!(
        resolve_windows_restricted_token_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &cwd,
            WindowsSandboxLevel::RestrictedToken,
        ),
        Ok(Some(WindowsSandboxFilesystemOverrides {
            read_roots_override: None,
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            additional_deny_read_paths: vec![],
            additional_deny_write_paths: expected_deny_write_paths,
        }))
    );
}

#[test]
fn windows_restricted_token_rejects_unreadable_split_carveouts() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let cwd = dunce::canonicalize(temp_dir.path())
        .expect("canonicalize temp dir")
        .abs();
    let blocked = cwd.join("blocked");
    std::fs::create_dir_all(blocked.as_path()).expect("create blocked");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::Root,
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::project_roots(
                    /*subpath*/ None,
                ),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path { path: blocked },
            access: codex_protocol::permissions::FileSystemAccessMode::Deny,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        resolve_windows_restricted_token_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &cwd,
            WindowsSandboxLevel::RestrictedToken,
        ),
        Err(
            "windows unelevated restricted-token sandbox cannot enforce deny-read restrictions directly; refusing to run unsandboxed"
                .to_string()
        )
    );
}

#[test]
fn windows_elevated_supports_split_restricted_read_roots() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let docs = temp_dir.path().join("docs");
    std::fs::create_dir_all(&docs).expect("create docs");
    let expected_docs = dunce::canonicalize(&docs).expect("canonical docs");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path {
                path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&docs)
                    .expect("absolute docs"),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        resolve_windows_elevated_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &temp_dir.path().abs(),
            /*use_windows_elevated_backend*/ true,
        ),
        Ok(Some(WindowsSandboxFilesystemOverrides {
            read_roots_override: Some(vec![expected_docs]),
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            additional_deny_read_paths: vec![],
            additional_deny_write_paths: vec![],
        }))
    );
}

#[test]
fn windows_elevated_supports_split_write_read_carveouts() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let docs = temp_dir.path().join("docs");
    std::fs::create_dir_all(&docs).expect("create docs");
    let expected_docs = dunce::canonicalize(&docs).expect("canonical docs");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::Root,
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::project_roots(
                    /*subpath*/ None,
                ),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path {
                path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&docs)
                    .expect("absolute docs"),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        resolve_windows_elevated_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &temp_dir.path().abs(),
            /*use_windows_elevated_backend*/ true,
        ),
        Ok(Some(WindowsSandboxFilesystemOverrides {
            read_roots_override: None,
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            additional_deny_read_paths: vec![],
            additional_deny_write_paths: vec![
                codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(expected_docs)
                    .expect("absolute docs"),
            ],
        }))
    );
}

#[test]
fn windows_elevated_supports_unreadable_split_carveouts() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let blocked = temp_dir.path().join("blocked");
    std::fs::create_dir_all(&blocked).expect("create blocked");
    let expected_blocked = dunce::canonicalize(&blocked).expect("canonical blocked");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::Root,
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::project_roots(
                    /*subpath*/ None,
                ),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path {
                path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&blocked)
                    .expect("absolute blocked"),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Deny,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        resolve_windows_elevated_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &temp_dir.path().abs(),
            /*use_windows_elevated_backend*/ true,
        ),
        Ok(Some(WindowsSandboxFilesystemOverrides {
            read_roots_override: None,
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            additional_deny_read_paths: vec![
                codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                    expected_blocked.clone(),
                )
                .expect("absolute blocked"),
            ],
            additional_deny_write_paths: vec![
                codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(expected_blocked)
                    .expect("absolute blocked"),
            ],
        }))
    );
}

#[test]
fn windows_elevated_supports_unreadable_globs() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let secret = temp_dir.path().join("app").join(".env");
    std::fs::create_dir_all(secret.parent().expect("parent")).expect("create parent");
    std::fs::write(&secret, "secret").expect("write secret");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::Root,
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::project_roots(
                    /*subpath*/ None,
                ),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::GlobPattern {
                pattern: "**/*.env".to_string(),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Deny,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        resolve_windows_elevated_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &temp_dir.path().abs(),
            /*use_windows_elevated_backend*/ true,
        ),
        Ok(Some(WindowsSandboxFilesystemOverrides {
            read_roots_override: None,
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            additional_deny_read_paths: vec![
                codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(secret)
                    .expect("absolute secret"),
            ],
            additional_deny_write_paths: vec![],
        }))
    );
}

#[test]
fn windows_elevated_rejects_reopened_writable_descendants() {
    let temp_dir = tempfile::TempDir::new().expect("tempdir");
    let docs = temp_dir.path().join("docs");
    let nested = docs.join("nested");
    std::fs::create_dir_all(&nested).expect("create nested");
    let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::Root,
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Special {
                value: codex_protocol::permissions::FileSystemSpecialPath::project_roots(
                    /*subpath*/ None,
                ),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path {
                path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&docs)
                    .expect("absolute docs"),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Read,
        },
        codex_protocol::permissions::FileSystemSandboxEntry {
            path: codex_protocol::permissions::FileSystemPath::Path {
                path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&nested)
                    .expect("absolute nested"),
            },
            access: codex_protocol::permissions::FileSystemAccessMode::Write,
        },
    ]);
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        NetworkSandboxPolicy::Restricted,
    );

    assert_eq!(
        unsupported_windows_restricted_token_sandbox_reason(
            SandboxType::WindowsRestrictedToken,
            &permission_profile,
            &temp_dir.path().abs(),
            WindowsSandboxLevel::Elevated,
        ),
        Some(
            "windows elevated sandbox cannot reopen writable descendants under read-only carveouts directly; refusing to run unsandboxed"
                .to_string()
        )
    );
}

#[cfg(windows)]
#[tokio::test]
async fn process_exec_tool_call_rejects_unavailable_sandbox_without_spawning() -> Result<()> {
    let temp_dir = tempfile::TempDir::new()?;
    let cwd = temp_dir.path().abs();
    let marker = temp_dir.path().join("unexpected-process.txt");
    let permission_profile = PermissionProfile::from_runtime_permissions(
        &FileSystemSandboxPolicy::unrestricted(),
        NetworkSandboxPolicy::Restricted,
    );
    let error = process_exec_tool_call(
        ExecParams {
            command: vec![
                "cmd.exe".to_string(),
                "/d".to_string(),
                "/c".to_string(),
                format!("echo unexpected execution > \"{}\"", marker.display()),
            ],
            codex_home: cwd.clone(),
            cwd: cwd.clone(),
            expiration: ExecExpiration::DefaultTimeout,
            capture_policy: ExecCapturePolicy::ShellTool,
            env: std::env::vars().collect(),
            network: None,
            network_environment_id: None,
            sandbox_permissions: SandboxPermissions::UseDefault,
            windows_sandbox_level: WindowsSandboxLevel::Disabled,
            windows_sandbox_private_desktop: false,
            justification: None,
            arg0: None,
        },
        &permission_profile,
        &cwd,
        &[],
        None,
    )
    .await
    .expect_err("network restrictions must not fall back to unsandboxed execution");
    assert!(
        matches!(error, CodexErr::Io(ref error) if error.kind() == std::io::ErrorKind::PermissionDenied)
    );
    assert!(
        error
            .to_string()
            .contains("no sandbox backend is available")
    );
    assert!(
        !marker.exists(),
        "rejected execution must not create a file"
    );
    Ok(())
}

#[tokio::test]
async fn build_exec_request_preserves_windows_workspace_roots() -> Result<()> {
    let temp_dir = tempfile::TempDir::new()?;
    let cwd = temp_dir.path().abs();
    let codex_home = temp_dir.path().join("configured-home").abs();
    let additional_root = temp_dir.path().join("additional").abs();
    let workspace_roots = vec![cwd.clone(), additional_root];

    let make_params = || ExecParams {
        command: vec!["echo".to_string(), "ok".to_string()],
        codex_home: codex_home.clone(),
        cwd: cwd.clone(),
        expiration: ExecExpiration::DefaultTimeout,
        capture_policy: ExecCapturePolicy::ShellTool,
        env: HashMap::new(),
        network: None,
        network_environment_id: None,
        sandbox_permissions: SandboxPermissions::UseDefault,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        justification: None,
        arg0: None,
    };
    let synchronous = build_exec_request(
        make_params(),
        &PermissionProfile::Disabled,
        &cwd,
        workspace_roots.as_slice(),
    )?;
    assert_eq!(synchronous.windows_sandbox_workspace_roots, workspace_roots);
    assert_eq!(synchronous.codex_home, codex_home);
    let exec_request = build_exec_request_async(
        make_params(),
        &PermissionProfile::Disabled,
        &cwd,
        workspace_roots.as_slice(),
    )
    .await?;

    assert_eq!(
        exec_request.windows_sandbox_workspace_roots,
        workspace_roots
    );
    assert_eq!(exec_request.codex_home, codex_home);
    let mut invalid = make_params();
    invalid.command.clear();
    let result = build_exec_request_async(
        invalid,
        &PermissionProfile::Disabled,
        &cwd,
        workspace_roots.as_slice(),
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("empty command must not produce an executable request"),
    };
    assert!(matches!(error, CodexErr::Io(error) if error.kind() == io::ErrorKind::InvalidInput));
    Ok(())
}

fn encode_powershell_script(script: &str) -> String {
    let utf16_le = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::prelude::BASE64_STANDARD.encode(utf16_le)
}

fn powershell_literal_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

#[tokio::test]
async fn direct_exec_cancellation_terminates_windows_descendants() -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    let temp_dir = tempfile::TempDir::new()?;
    let ready_marker = temp_dir.path().join("descendant.ready");
    let survival_marker = temp_dir.path().join("descendant.survived");
    let descendant_script = format!(
        "[System.IO.File]::WriteAllText('{}', [string]$PID); Start-Sleep -Seconds 2; Set-Content -LiteralPath '{}' -Value survived",
        powershell_literal_path(&ready_marker),
        powershell_literal_path(&survival_marker)
    );
    let descendant_script = encode_powershell_script(&descendant_script);
    let root_script = format!(
        "$ErrorActionPreference = 'Stop'; \
         $null = Start-Process -FilePath 'powershell.exe' \
             -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand','{descendant_script}') \
             -WindowStyle Hidden; \
         Start-Sleep -Seconds 60"
    );
    let command = vec![
        "powershell.exe".to_string(),
        "-NoLogo".to_string(),
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-EncodedCommand".to_string(),
        encode_powershell_script(&root_script),
    ];
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir()?;
    let cancellation = CancellationToken::new();
    let cancel_tx = cancellation.clone();
    let ready_for_cancel = ready_marker.clone();
    let cancel_task = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let descendant = loop {
            let descendant = std::fs::read_to_string(&ready_for_cancel)
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
                .and_then(|pid| {
                    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
                    (!raw.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(raw) })
                });
            if descendant.is_some() || tokio::time::Instant::now() >= deadline {
                break descendant;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        if let Some(handle) = &descendant {
            assert_eq!(
                unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) },
                WAIT_TIMEOUT
            );
        }
        cancel_tx.cancel();
        descendant
    });
    let params = ExecParams {
        command,
        codex_home: cwd.clone(),
        cwd,
        expiration: ExecExpiration::Cancellation(cancellation),
        capture_policy: ExecCapturePolicy::ShellTool,
        env: std::env::vars().collect(),
        network: None,
        network_environment_id: None,
        sandbox_permissions: SandboxPermissions::UseDefault,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        justification: None,
        arg0: None,
    };

    let output = timeout(
        Duration::from_secs(15),
        exec(
            params,
            NetworkSandboxPolicy::Restricted,
            /*stdout_stream*/ None,
            /*after_spawn*/ None,
        ),
    )
    .await
    .expect("Windows direct exec cancellation should complete promptly")?;
    let descendant = cancel_task
        .await
        .expect("join cancellation task")
        .expect("actual descendant must start before cancellation");
    timeout(Duration::from_secs(5), async {
        while unsafe { WaitForSingleObject(descendant.as_raw_handle(), 0) } != WAIT_OBJECT_0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("direct exec cancellation must confirm native descendant exit");
    assert!(!output.timed_out);

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !survival_marker.exists(),
        "Windows direct exec descendant survived cancellation"
    );
    Ok(())
}

#[tokio::test]
async fn process_exec_tool_call_respects_cancellation_token() -> Result<()> {
    let command = long_running_command();
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir()?;
    let env: HashMap<String, String> = std::env::vars().collect();
    let cancel_token = CancellationToken::new();
    let cancel_tx = cancel_token.clone();
    let params = ExecParams {
        command,
        codex_home: cwd.clone(),
        cwd: cwd.clone(),
        expiration: ExecExpiration::Cancellation(cancel_token),
        capture_policy: ExecCapturePolicy::ShellTool,
        env,
        network: None,
        network_environment_id: None,
        sandbox_permissions: SandboxPermissions::UseDefault,
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        justification: None,
        arg0: None,
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        cancel_tx.cancel();
    });
    let result = timeout(
        Duration::from_secs(5),
        process_exec_tool_call(
            params,
            &PermissionProfile::Disabled,
            &cwd,
            std::slice::from_ref(&cwd),
            /*stdout_stream*/ None,
        ),
    )
    .await
    .expect("cancellation should stop the process promptly");
    let output = result.expect("cancellation should return a non-timeout exec result");
    assert!(!output.timed_out);
    assert_eq!(output.exit_code, 130);
    assert!(output.stderr.text.contains("Command cancelled."));
    assert!(output.aggregated_output.text.contains("Command cancelled."));
    Ok(())
}

fn long_running_command() -> Vec<String> {
    vec![
        "powershell.exe".to_string(),
        "-NonInteractive".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Seconds 30".to_string(),
    ]
}

#[tokio::test]
async fn audit_reader_failure_preserves_partial_capture() {
    let stdout = Arc::new(std::sync::Mutex::new(OutputCapture::new(None)));
    let stderr = Arc::new(std::sync::Mutex::new(OutputCapture::new(None)));
    let aggregate = Arc::new(std::sync::Mutex::new(OutputCapture::new(None)));
    stdout.lock().unwrap().append(b"committed output");
    aggregate.lock().unwrap().append(b"committed output");
    let output = await_captured_output_until_deadline(
        tokio::spawn(async { Err(io::Error::other("injected reader failure")) }),
        tokio::spawn(async { Ok(()) }),
        stdout,
        stderr,
        Arc::clone(&aggregate),
        tokio::time::Instant::now() + Duration::from_secs(1),
    )
    .await
    .expect("capture failures retain execution evidence");
    assert!(output.0.truncated);
    assert!(String::from_utf8_lossy(&output.0.text).contains("committed output"));
    assert!(String::from_utf8_lossy(&output.0.text).contains("injected reader failure"));
    assert!(aggregate.lock().unwrap().snapshot().truncated);
}
