use std::collections::HashMap;
use std::path::Path;

use pretty_assertions::assert_eq;

use crate::ProcessDriver;
use crate::SpawnedProcess;
use crate::TerminalSize;
use crate::combine_output_receivers;

use crate::spawn_from_driver;
use crate::spawn_pipe_process;
use crate::spawn_pipe_process_no_stdin;
use crate::spawn_pty_process;

#[path = "windows_tests.rs"]
mod windows_tests;

fn find_python() -> Option<String> {
    for candidate in ["python3", "python"] {
        if let Ok(output) = std::process::Command::new(candidate)
            .arg("--version")
            .output()
            && output.status.success()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

fn shell_command(program: &str) -> (String, Vec<String>) {
    let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    (cmd, vec!["/C".to_string(), program.to_string()])
}

fn echo_sleep_command(marker: &str) -> String {
    format!("echo {marker} & ping -n 2 127.0.0.1 > NUL")
}

fn split_stdout_stderr_command() -> String {
    // Keep this in cmd.exe syntax so the test does not depend on a runner-local
    // PowerShell/Python setup just to produce deterministic split output.
    "(echo split-out)&(>&2 echo split-err)".to_string()
}

async fn collect_split_output(mut output_rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    let mut collected = Vec::new();
    while let Some(chunk) = output_rx.recv().await {
        collected.extend_from_slice(&chunk);
    }
    collected
}

fn combine_spawned_output(
    spawned: SpawnedProcess,
) -> (
    crate::ProcessHandle,
    tokio::sync::broadcast::Receiver<Vec<u8>>,
    tokio::sync::oneshot::Receiver<i32>,
) {
    let SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;
    (
        session,
        combine_output_receivers(stdout_rx, stderr_rx),
        exit_rx,
    )
}

async fn collect_output_until_exit(
    mut output_rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
    exit_rx: tokio::sync::oneshot::Receiver<i32>,
    timeout_ms: u64,
) -> (Vec<u8>, i32) {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    tokio::pin!(exit_rx);

    loop {
        tokio::select! {
            res = output_rx.recv() => {
                if let Ok(chunk) = res {
                    collected.extend_from_slice(&chunk);
                }
            }
            res = &mut exit_rx => {
                let code = res.unwrap_or(-1);
                // On Windows (ConPTY in particular), it's possible to observe the exit notification
                // before the final bytes are drained from the PTY reader thread. Drain for a brief
                // "quiet" window to make output assertions deterministic.
                let (quiet_ms, max_ms) = (200, 2_000);
                let quiet = tokio::time::Duration::from_millis(quiet_ms);
                let max_deadline =
                    tokio::time::Instant::now() + tokio::time::Duration::from_millis(max_ms);
                while tokio::time::Instant::now() < max_deadline {
                    match tokio::time::timeout(quiet, output_rx.recv()).await {
                        Ok(Ok(chunk)) => collected.extend_from_slice(&chunk),
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                        Err(_) => break,
                    }
                }
                return (collected, code);
            }
            _ = tokio::time::sleep_until(deadline) => {
                return (collected, -1);
            }
        }
    }
}

async fn wait_for_output_contains(
    output_rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>,
    needle: &str,
    timeout_ms: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);

    while tokio::time::Instant::now() < deadline {
        let now = tokio::time::Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        match tokio::time::timeout(remaining, output_rx.recv()).await {
            Ok(Ok(chunk)) => {
                collected.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&collected).contains(needle) {
                    return Ok(collected);
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                anyhow::bail!(
                    "PTY output closed while waiting for {needle:?}: {:?}",
                    String::from_utf8_lossy(&collected)
                );
            }
            Err(_) => break,
        }
    }

    anyhow::bail!(
        "timed out waiting for {needle:?} in PTY output: {:?}",
        String::from_utf8_lossy(&collected)
    );
}

async fn wait_for_python_repl_ready(
    output_rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>,
    timeout_ms: u64,
    ready_marker: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);

    while tokio::time::Instant::now() < deadline {
        let now = tokio::time::Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        match tokio::time::timeout(remaining, output_rx.recv()).await {
            Ok(Ok(chunk)) => {
                collected.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&collected).contains(ready_marker) {
                    return Ok(collected);
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                anyhow::bail!(
                    "PTY output closed while waiting for Python REPL readiness: {:?}",
                    String::from_utf8_lossy(&collected)
                );
            }
            Err(_) => break,
        }
    }

    anyhow::bail!(
        "timed out waiting for Python REPL readiness marker {ready_marker:?} in PTY: {:?}",
        String::from_utf8_lossy(&collected)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pty_python_repl_emits_output_and_exits() -> anyhow::Result<()> {
    let Some(python) = find_python() else {
        eprintln!("python not found; skipping pty_python_repl_emits_output_and_exits");
        return Ok(());
    };

    let ready_marker = "__codex_pty_ready__";
    let args = vec![
        "-i".to_string(),
        "-q".to_string(),
        "-c".to_string(),
        format!("print('{ready_marker}')"),
    ];
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let spawned = spawn_pty_process(
        &python,
        &args,
        Path::new("."),
        &env_map,
        &None,
        TerminalSize::default(),
    )
    .await?;
    let (session, mut output_rx, exit_rx) = combine_spawned_output(spawned);
    let writer = session.writer_sender();
    let newline = "\r\n";
    let startup_timeout_ms = 10_000;
    let mut output =
        wait_for_python_repl_ready(&mut output_rx, startup_timeout_ms, ready_marker).await?;
    writer
        .send(format!("print('hello from pty'){newline}").into_bytes())
        .await?;
    writer.send(format!("exit(){newline}").into_bytes()).await?;

    let timeout_ms = 10_000;
    let (remaining_output, code) = collect_output_until_exit(output_rx, exit_rx, timeout_ms).await;
    output.extend_from_slice(&remaining_output);
    let text = String::from_utf8_lossy(&output);

    assert!(
        text.contains("hello from pty"),
        "expected python output in PTY: {text:?}"
    );
    assert_eq!(code, 0, "expected python to exit cleanly");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_process_round_trips_stdin() -> anyhow::Result<()> {
    let (program, args) = {
        let cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        (
            cmd,
            vec![
                "/Q".to_string(),
                "/V:ON".to_string(),
                "/D".to_string(),
                "/C".to_string(),
                "set /p line= & echo(!line!".to_string(),
            ],
        )
    };
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let spawned = spawn_pipe_process(&program, &args, Path::new("."), &env_map, &None).await?;
    let (session, output_rx, exit_rx) = combine_spawned_output(spawned);
    let writer = session.writer_sender();
    let newline = "\r\n";
    writer
        .send(format!("roundtrip{newline}").into_bytes())
        .await?;
    drop(writer);
    session.close_stdin();

    let (output, code) = collect_output_until_exit(output_rx, exit_rx, /*timeout_ms*/ 5_000).await;
    let text = String::from_utf8_lossy(&output);

    assert!(
        text.contains("roundtrip"),
        "expected pipe process to echo stdin: {text:?}"
    );
    assert_eq!(code, 0, "expected python -c to exit cleanly");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_and_pty_share_interface() -> anyhow::Result<()> {
    let env_map: HashMap<String, String> = std::env::vars().collect();

    let (pipe_program, pipe_args) = shell_command(&echo_sleep_command("pipe_ok"));
    let (pty_program, pty_args) = shell_command(&echo_sleep_command("pty_ok"));

    let pipe =
        spawn_pipe_process(&pipe_program, &pipe_args, Path::new("."), &env_map, &None).await?;
    let pty = spawn_pty_process(
        &pty_program,
        &pty_args,
        Path::new("."),
        &env_map,
        &None,
        TerminalSize::default(),
    )
    .await?;
    let (_pipe_session, pipe_output_rx, pipe_exit_rx) = combine_spawned_output(pipe);
    let (_pty_session, pty_output_rx, pty_exit_rx) = combine_spawned_output(pty);

    let timeout_ms = 10_000;
    let (pipe_out, pipe_code) =
        collect_output_until_exit(pipe_output_rx, pipe_exit_rx, timeout_ms).await;
    let (pty_out, pty_code) =
        collect_output_until_exit(pty_output_rx, pty_exit_rx, timeout_ms).await;

    assert_eq!(pipe_code, 0);
    assert_eq!(pty_code, 0);
    assert!(
        String::from_utf8_lossy(&pipe_out).contains("pipe_ok"),
        "pipe output mismatch: {pipe_out:?}"
    );
    assert!(
        String::from_utf8_lossy(&pty_out).contains("pty_ok"),
        "pty output mismatch: {pty_out:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_drains_stderr_without_stdout_activity() -> anyhow::Result<()> {
    let Some(python) = find_python() else {
        eprintln!("python not found; skipping pipe_drains_stderr_without_stdout_activity");
        return Ok(());
    };

    let script = "import sys\nchunk = 'E' * 65536\nfor _ in range(64):\n    sys.stderr.write(chunk)\n    sys.stderr.flush()\n";
    let args = vec!["-c".to_string(), script.to_string()];
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let spawned = spawn_pipe_process(&python, &args, Path::new("."), &env_map, &None).await?;
    let (_session, output_rx, exit_rx) = combine_spawned_output(spawned);

    let (output, code) = collect_output_until_exit(output_rx, exit_rx, /*timeout_ms*/ 10_000).await;

    assert_eq!(code, 0, "expected python to exit cleanly");
    assert!(!output.is_empty(), "expected stderr output to be drained");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_process_can_expose_split_stdout_and_stderr() -> anyhow::Result<()> {
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let (program, args) = shell_command(&split_stdout_stderr_command());
    let spawned =
        spawn_pipe_process_no_stdin(&program, &args, Path::new("."), &env_map, &None).await?;
    let SpawnedProcess {
        session: _session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;

    let timeout_ms = 10_000;
    let timeout = tokio::time::Duration::from_millis(timeout_ms);
    let stdout_task = tokio::spawn(async move { collect_split_output(stdout_rx).await });
    let stderr_task = tokio::spawn(async move { collect_split_output(stderr_rx).await });
    let code = tokio::time::timeout(timeout, exit_rx)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for split process exit"))?
        .unwrap_or(-1);
    let stdout = tokio::time::timeout(timeout, stdout_task)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting to drain split stdout"))??;
    let stderr = tokio::time::timeout(timeout, stderr_task)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting to drain split stderr"))??;

    let expected_stdout = b"split-out\r\n".to_vec();
    let expected_stderr = b"split-err\r\n".to_vec();

    assert_eq!(stdout, expected_stdout);
    assert_eq!(stderr, expected_stderr);
    assert_eq!(code, 0);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_backed_process_can_expose_split_stdout_and_stderr() -> anyhow::Result<()> {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_driver_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (stderr_tx, stderr_driver_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();

    let spawned = spawn_from_driver(ProcessDriver {
        writer_tx,
        stdout_rx: stdout_driver_rx,
        stderr_rx: Some(stderr_driver_rx),
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
    });

    let SpawnedProcess {
        session: _session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;
    let stdout_task = tokio::spawn(async move { collect_split_output(stdout_rx).await });
    let stderr_task = tokio::spawn(async move { collect_split_output(stderr_rx).await });

    stdout_tx.send(b"driver-out".to_vec())?;
    stderr_tx.send(b"driver-err".to_vec())?;
    drop(stdout_tx);
    drop(stderr_tx);
    exit_tx.send(0).expect("send exit code");

    let timeout = tokio::time::Duration::from_secs(2);
    let code = tokio::time::timeout(timeout, exit_rx)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for driver exit"))?
        .unwrap_or(-1);
    let stdout = tokio::time::timeout(timeout, stdout_task)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting to drain driver stdout"))??;
    let stderr = tokio::time::timeout(timeout, stderr_task)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting to drain driver stderr"))??;

    assert_eq!(stdout, b"driver-out".to_vec());
    assert_eq!(stderr, b"driver-err".to_vec());
    assert_eq!(code, 0);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_backed_process_can_resize_via_resizer_hook() -> anyhow::Result<()> {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (_stdout_tx, stdout_driver_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
    let (size_tx, size_rx) = tokio::sync::oneshot::channel::<TerminalSize>();

    let size_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(size_tx)));
    let spawned = spawn_from_driver(ProcessDriver {
        writer_tx,
        stdout_rx: stdout_driver_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: Some(Box::new(move |size| {
            if let Ok(mut guard) = size_tx.lock()
                && let Some(size_tx) = guard.take()
            {
                let _ = size_tx.send(size);
            }
            Ok(())
        })),
    });

    spawned.session.resize(TerminalSize {
        rows: 40,
        cols: 120,
    })?;
    exit_tx.send(0).expect("send exit code");

    let resized = tokio::time::timeout(tokio::time::Duration::from_secs(2), size_rx)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for resize"))?
        .expect("receive resized terminal size");
    assert_eq!(
        resized,
        TerminalSize {
            rows: 40,
            cols: 120
        }
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_backed_process_drains_output_that_arrives_after_exit_signal() -> anyhow::Result<()>
{
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_driver_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();

    let spawned = spawn_from_driver(ProcessDriver {
        writer_tx,
        stdout_rx: stdout_driver_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
    });

    let SpawnedProcess {
        session: _session,
        stdout_rx,
        stderr_rx: _stderr_rx,
        exit_rx,
    } = spawned;
    let stdout_task = tokio::spawn(async move { collect_split_output(stdout_rx).await });

    exit_tx.send(0).expect("send exit code");
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    stdout_tx.send(b"tail".to_vec())?;
    drop(stdout_tx);

    let timeout = tokio::time::Duration::from_secs(2);
    let code = tokio::time::timeout(timeout, exit_rx)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for driver exit"))?
        .unwrap_or(-1);
    let stdout = tokio::time::timeout(timeout, stdout_task)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting to drain driver stdout"))??;

    assert_eq!(stdout, b"tail".to_vec());
    assert_eq!(code, 0);

    Ok(())
}
