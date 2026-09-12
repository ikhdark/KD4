use super::*;

use base64::Engine;
use codex_exec_server::ExecBackend;
use codex_exec_server::ExecBackendFuture;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use tempfile::tempdir;
use tokio::sync::watch;
use tokio::time::Duration;

struct DelayedSnapshotBackend {
    process: Arc<DelayedSnapshotProcess>,
    start_delay: Duration,
}

impl ExecBackend for DelayedSnapshotBackend {
    fn start(&self, _params: ExecParams) -> ExecBackendFuture<'_> {
        Box::pin(async move {
            tokio::time::sleep(self.start_delay).await;
            Ok(StartedExecProcess {
                process: self.process.clone(),
            })
        })
    }
}

struct DelayedSnapshotProcess {
    process_id: ProcessId,
    read_delay: Duration,
    terminate_calls: AtomicUsize,
    wake_tx: watch::Sender<u64>,
}

impl ExecProcess for DelayedSnapshotProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(async move {
            tokio::time::sleep(self.read_delay).await;
            Ok(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: true,
                exit_code: Some(0),
                closed: true,
                failure: None,
                sandbox_denied: false,
            })
        })
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async {
            Ok(WriteResponse {
                status: WriteStatus::Accepted,
            })
        })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(async move {
            self.terminate_calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
    }
}

fn current_environment() -> HashMap<String, String> {
    std::env::vars().collect()
}

#[tokio::test(start_paused = true)]
async fn remote_snapshot_start_and_collection_share_one_deadline() {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let process = Arc::new(DelayedSnapshotProcess {
        process_id: "snapshot-process".into(),
        read_delay: Duration::from_secs(6),
        terminate_calls: AtomicUsize::new(0),
        wake_tx,
    });
    let backend: Arc<dyn ExecBackend> = Arc::new(DelayedSnapshotBackend {
        process: process.clone(),
        start_delay: Duration::from_secs(6),
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let result = run_remote_snapshot_process_before(
        backend,
        ExecParams {
            process_id: process.process_id.clone(),
            argv: vec!["snapshot".to_string()],
            cwd: PathUri::from_host_native_path(std::env::temp_dir())
                .expect("temporary directory should be absolute"),
            env_policy: None,
            env: HashMap::new(),
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
        },
        deadline,
        "test-shell",
    )
    .await;

    assert_eq!(
        result
            .expect_err("combined start/read work should exceed one deadline")
            .to_string(),
        "Snapshot command timed out for test-shell"
    );
    assert_eq!(process.terminate_calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn remote_snapshot_build_terminates_failed_capture_before_returning() -> Result<()> {
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;

    // The peer replaces the external executor only. Capture, environment
    // selection, remote process ownership, and snapshot admission remain real.
    for failure_kind in ["read_error", "process_failure", "output_overflow"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            let mut websocket = tokio_tungstenite::accept_async(socket).await?;
            let mut methods = Vec::new();
            let mut process_id = None;
            while let Some(message) = websocket.next().await {
                let request: serde_json::Value = serde_json::from_slice(&message?.into_data())?;
                let method = request["method"].as_str().context("request method")?;
                methods.push(method.to_string());
                if method == "initialized" {
                    continue;
                }
                let result = match method {
                    "initialize" => json!({"sessionId": "snapshot-cleanup-test"}),
                    "environment/info" => json!({
                        "operatingSystem": "windows",
                        "shell": {"name": "cmd", "path": "cmd.exe"},
                        "cwd": "file:///C:/workspace"
                    }),
                    "fs/createDirectory" => json!({}),
                    "process/start" => {
                        process_id = Some(request["params"]["processId"].clone());
                        json!({"processId": process_id})
                    }
                    "process/read" => {
                        assert_eq!(Some(&request["params"]["processId"]), process_id.as_ref());
                        if failure_kind == "read_error" {
                            websocket
                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                    json!({"jsonrpc": "2.0", "id": request["id"],
                                    "error": {"code": -32000, "message": "snapshot read failed"}})
                                    .to_string()
                                    .into(),
                                ))
                                .await?;
                            continue;
                        }
                        serde_json::to_value(ReadResponse {
                            chunks: if failure_kind == "output_overflow" {
                                vec![codex_exec_server::ProcessOutputChunk {
                                    seq: 0,
                                    stream: ExecOutputStream::Stdout,
                                    chunk: vec![b'x'; SNAPSHOT_OUTPUT_LIMIT_BYTES + 1].into(),
                                }]
                            } else {
                                Vec::new()
                            },
                            next_seq: 1,
                            exited: false,
                            exit_code: None,
                            closed: false,
                            failure: (failure_kind == "process_failure")
                                .then(|| "snapshot process failed".to_string()),
                            sandbox_denied: false,
                        })?
                    }
                    "process/terminate" => {
                        assert_eq!(Some(&request["params"]["processId"]), process_id.as_ref());
                        json!({"running": false})
                    }
                    other => anyhow::bail!("unexpected request after failed capture: {other}"),
                };
                websocket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                            .to_string()
                            .into(),
                    ))
                    .await?;
                if method == "process/terminate" {
                    return Ok::<_, anyhow::Error>(methods);
                }
            }
            anyhow::bail!("snapshot returned without terminating failed capture")
        });
        let directory = tempdir()?;
        let environment = Arc::new(Environment::create_for_tests(Some(format!(
            "ws://{address}"
        )))?);
        let session_id = ThreadId::new();
        let snapshot = ShellSnapshot::new(
            directory.path().abs(),
            session_id,
            SessionTelemetry::new(
                session_id,
                "test-model",
                "test-model",
                None,
                None,
                None,
                "test".to_string(),
                false,
                "unknown".to_string(),
                codex_protocol::protocol::SessionSource::Cli,
            ),
            None,
            HashMap::new(),
            ShellEnvironmentPolicy::default(),
        );
        let result = timeout(
            Duration::from_secs(20),
            snapshot.build(TurnEnvironment::new(
                "remote".to_string(),
                environment,
                PathUri::from_abs_path(&directory.path().abs()),
                Some(Shell {
                    shell_type: ShellType::Cmd,
                    shell_path: "cmd.exe".into(),
                }),
            )),
        )
        .await?;
        assert!(
            result.is_none(),
            "failed capture must not publish a snapshot: {failure_kind}"
        );
        let methods = timeout(Duration::from_secs(5), peer).await???;
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "environment/info",
                "fs/createDirectory",
                "process/start",
                "process/read",
                "process/terminate"
            ],
            "failed capture must terminate exactly once and never write or validate a snapshot: {failure_kind}"
        );
    }
    Ok(())
}

fn assert_snapshot_section(snapshot: &str, section: &str) {
    assert!(
        snapshot.lines().any(|line| line == section),
        "snapshot should contain exact section header {section:?}; snapshot={snapshot:?}"
    );
}

#[test]
fn strip_snapshot_preamble_removes_leading_output() {
    let snapshot = "noise\n# Snapshot file\nexport PATH=/bin\n";
    let cleaned = strip_snapshot_preamble(snapshot).expect("snapshot marker exists");
    assert_eq!(cleaned, "# Snapshot file\nexport PATH=/bin\n");
}

#[test]
fn strip_snapshot_preamble_requires_marker() {
    let result = strip_snapshot_preamble("missing header");
    assert!(result.is_err());
}

#[test]
fn snapshot_file_name_parser_supports_legacy_and_suffixed_names() {
    let session_id = "019cf82b-6a62-7700-bbbd-46909794ef89";

    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.sh")),
        None
    );
    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.123.sh")),
        None
    );
    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.tmp-123")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.tmp-123.ps1")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.789.cmd")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name("not-a-snapshot.txt"),
        None
    );
}

#[tokio::test]
async fn non_windows_shell_snapshot_is_rejected_before_writing() -> Result<()> {
    let dir = tempdir()?;
    let codex_home = dir.path().abs();
    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: "bash".into(),
    };

    let result = ShellSnapshot::try_create(
        &codex_home,
        ThreadId::new(),
        &codex_home,
        &shell,
        &current_environment(),
        /*state_db*/ None,
    )
    .await;

    assert!(matches!(result, Err("unsupported_shell")));
    assert!(!codex_home.join(SNAPSHOT_DIR).exists());
    Ok(())
}

#[test]
fn cmd_snapshot_formats_environment_as_replayable_batch() -> Result<()> {
    let raw = "profile noise\r\n# Snapshot file\r\n# Codex Cmd snapshot format: 1\r\n# exports\r\nCODEX_TEST=100%^value\r\nCODEX_META=quoted\" & piped| angles<> parens()\r\nPWD=C:\\ignored\r\n__CODEX_PRIVATE=ignored\r\n";
    let snapshot = format_snapshot(ShellType::Cmd, raw)?;
    assert_snapshot_section(&snapshot, CMD_SNAPSHOT_FORMAT_HEADER);
    assert!(snapshot.contains("@set CODEX_TEST=100%%^^value"));
    assert!(
        snapshot.contains("@set CODEX_META=quoted^\" ^& piped^| angles^<^> parens^(^)"),
        "snapshot should escape Cmd metacharacters: {snapshot:?}"
    );
    assert!(!snapshot.contains("PWD="));
    assert!(!snapshot.contains("__CODEX_PRIVATE="));
    Ok(())
}

#[tokio::test]
async fn windows_cmd_snapshot_captures_validates_and_replays_environment() -> Result<()> {
    let shell = crate::shell::get_shell(ShellType::Cmd, /*path*/ None)
        .context("Cmd is required for snapshot test")?;
    let dir = tempdir()?;
    let cwd = dir.path().abs();
    let marker_name = "CODEX_SNAPSHOT_CMD_TEST";
    let marker_value = "100%^&! quoted";
    let mut capture_environment = current_environment();
    capture_environment.insert(marker_name.to_string(), marker_value.to_string());
    let snapshot_file = ShellSnapshot::try_create(
        &cwd,
        ThreadId::new(),
        &cwd,
        &shell,
        &capture_environment,
        /*state_db*/ None,
    )
    .await
    .expect("Cmd snapshot should be captured, validated, and finalized");
    let snapshot_path = snapshot_file.path();
    assert_eq!(
        snapshot_path
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("cmd")
    );
    let snapshot = fs::read_to_string(&snapshot_path).await?;
    assert_snapshot_section(&snapshot, CMD_SNAPSHOT_FORMAT_HEADER);

    let replay_environment =
        parse_cmd_snapshot_environment(&snapshot).expect("captured Cmd snapshot should parse");
    assert_eq!(
        replay_environment
            .iter()
            .find(|(name, _)| name == marker_name)
            .map(|(_, value)| value.as_str()),
        Some(marker_value)
    );
    Ok(())
}

#[tokio::test]
async fn windows_powershell_snapshot_includes_sections() -> Result<()> {
    let shell = crate::shell::get_shell(ShellType::PowerShell, /*path*/ None)
        .context("PowerShell is required for snapshot test")?;
    let dir = tempdir()?;
    let cwd = dir.path().abs();
    let marker_name = "CODEX_SNAPSHOT_WINDOWS_UNICODE_TEST";
    let marker_value = "snowman-雪-'quoted'";
    let mut capture_environment = current_environment();
    capture_environment.insert(marker_name.to_string(), marker_value.to_string());
    let snapshot_file = ShellSnapshot::try_create(
        &cwd,
        ThreadId::new(),
        &cwd,
        &shell,
        &capture_environment,
        /*state_db*/ None,
    )
    .await
    .expect("PowerShell snapshot should be captured, validated, and finalized");
    let snapshot_path = snapshot_file.path();
    assert_eq!(
        snapshot_path
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("ps1")
    );
    let snapshot = fs::read_to_string(&snapshot_path).await?;
    for section in ["# Snapshot file", "# Functions", "# aliases", "# exports"] {
        assert_snapshot_section(&snapshot, section);
    }
    assert_snapshot_section(&snapshot, POWERSHELL_SNAPSHOT_FORMAT_HEADER);

    let mut replay_environment = current_environment();
    replay_environment.remove(marker_name);
    let snapshot_path = powershell_single_quote(&snapshot_path.to_string_lossy());
    let replay = run_script_with_timeout(
        &shell,
        &format!(
            "try {{ [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 }} catch {{}}; . '{snapshot_path}'; Microsoft.PowerShell.Utility\\Write-Output $env:{marker_name}"
        ),
        SNAPSHOT_TIMEOUT,
        /*use_login_shell*/ false,
        &cwd,
        &replay_environment,
    )
    .await?;
    assert_eq!(replay.trim(), marker_value);
    Ok(())
}

#[tokio::test]
async fn windows_snapshot_timeout_terminates_descendants() -> Result<()> {
    let shell = crate::shell::get_shell(ShellType::PowerShell, /*path*/ None)
        .context("PowerShell is required for snapshot test")?;
    let dir = tempdir()?;
    let cwd = dir.path().abs();
    let ready_marker = cwd.join("descendant.ready");
    let survival_marker = cwd.join("descendant.survived");
    let descendant_script = format!(
        "Start-Sleep -Seconds 2; Set-Content -LiteralPath '{}' -Value survived",
        powershell_single_quote(&survival_marker.to_string_lossy())
    );
    let descendant_script = base64::prelude::BASE64_STANDARD.encode(
        descendant_script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let root_script = format!(
        "$null = Start-Process -FilePath 'powershell.exe' \
             -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand','{descendant_script}') \
             -WindowStyle Hidden; \
         Set-Content -LiteralPath '{}' -Value ready; \
         Start-Sleep -Seconds 60",
        powershell_single_quote(&ready_marker.to_string_lossy())
    );

    let result = run_script_with_timeout(
        &shell,
        &root_script,
        Duration::from_secs(1),
        /*use_login_shell*/ false,
        &cwd,
        &current_environment(),
    )
    .await;

    assert!(result.is_err(), "snapshot command should time out");
    assert!(
        ready_marker.exists(),
        "snapshot root did not launch its descendant before timing out"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !survival_marker.exists(),
        "snapshot descendant survived the timeout"
    );
    Ok(())
}

async fn write_rollout_stub(codex_home: &Path, session_id: ThreadId) -> Result<PathBuf> {
    let dir = codex_home
        .join("sessions")
        .join("2025")
        .join("01")
        .join("01");
    fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("rollout-2025-01-01T00-00-00-{session_id}.jsonl"));
    fs::write(&path, "").await?;
    Ok(path)
}

#[tokio::test]
async fn cleanup_stale_snapshots_removes_orphans_and_keeps_live() -> Result<()> {
    let dir = tempdir()?;
    let codex_home = dir.path().abs();
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
    fs::create_dir_all(&snapshot_dir).await?;

    let live_session = ThreadId::new();
    let orphan_session = ThreadId::new();
    let live_snapshot = snapshot_dir.join(format!("{live_session}.123.cmd"));
    let orphan_snapshot = snapshot_dir.join(format!("{orphan_session}.456.cmd"));
    let invalid_snapshot = snapshot_dir.join("not-a-snapshot.txt");

    write_rollout_stub(&codex_home, live_session).await?;
    fs::write(&live_snapshot, "live").await?;
    fs::write(&orphan_snapshot, "orphan").await?;
    fs::write(&invalid_snapshot, "invalid").await?;

    cleanup_stale_snapshots(&codex_home, ThreadId::new(), /*state_db*/ None).await?;

    assert_eq!(live_snapshot.exists(), true);
    assert_eq!(orphan_snapshot.exists(), false);
    assert_eq!(invalid_snapshot.exists(), false);
    Ok(())
}

#[tokio::test]
async fn cancelled_remote_snapshot_build_terminates_capture_without_publishing_file() -> Result<()>
{
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (read_started, read_observed) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        let mut websocket = tokio_tungstenite::accept_async(socket).await?;
        let mut read_started = Some(read_started);
        let mut methods = Vec::new();
        let mut process_id = None;
        while let Some(message) = websocket.next().await {
            let request: serde_json::Value = serde_json::from_slice(&message?.into_data())?;
            let method = request["method"].as_str().context("request method")?;
            methods.push(method.to_string());
            if method == "initialized" {
                continue;
            }
            let result = match method {
                "initialize" => json!({"sessionId": "snapshot-cancellation-test"}),
                "environment/info" => json!({
                    "operatingSystem": "windows",
                    "shell": {"name": "cmd", "path": "cmd.exe"},
                    "cwd": "file:///C:/workspace"
                }),
                "fs/createDirectory" => json!({}),
                "process/start" => {
                    process_id = Some(request["params"]["processId"].clone());
                    json!({"processId": process_id})
                }
                "process/read" => {
                    assert_eq!(Some(&request["params"]["processId"]), process_id.as_ref());
                    read_started
                        .take()
                        .expect("one in-flight capture read")
                        .send(())
                        .expect("capture caller awaits read admission");
                    // Model an external shell that remains running. The real client
                    // must cancel this wait and send terminate after its owner is dropped.
                    continue;
                }
                "process/terminate" => {
                    assert_eq!(Some(&request["params"]["processId"]), process_id.as_ref());
                    json!({"running": false})
                }
                other => anyhow::bail!(
                    "cancelled capture must not publish or validate a snapshot: {other}"
                ),
            };
            websocket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    json!({"jsonrpc":"2.0", "id": request["id"], "result": result})
                        .to_string()
                        .into(),
                ))
                .await?;
            if method == "process/terminate" {
                return Ok::<_, anyhow::Error>(methods);
            }
        }
        anyhow::bail!("remote capture owner vanished without terminating its running process")
    });
    let directory = tempdir()?;
    let environment = Arc::new(Environment::create_for_tests(Some(format!(
        "ws://{address}"
    )))?);
    let session_id = ThreadId::new();
    let snapshot = ShellSnapshot::new(
        directory.path().abs(),
        session_id,
        SessionTelemetry::new(
            session_id,
            "test-model",
            "test-model",
            None,
            None,
            None,
            "test".to_string(),
            false,
            "unknown".to_string(),
            codex_protocol::protocol::SessionSource::Cli,
        ),
        None,
        HashMap::new(),
        ShellEnvironmentPolicy::default(),
    );
    let capture = tokio::spawn(snapshot.build(TurnEnvironment::new(
        "remote".to_string(),
        environment,
        PathUri::from_abs_path(&directory.path().abs()),
        Some(Shell {
            shell_type: ShellType::Cmd,
            shell_path: "cmd.exe".into(),
        }),
    )));
    timeout(Duration::from_secs(5), read_observed).await??;
    capture.abort();
    assert!(
        capture
            .await
            .err()
            .expect("outer snapshot waiter is cancelled")
            .is_cancelled()
    );
    // This is deliberately shorter than SNAPSHOT_TIMEOUT: cancellation must
    // trigger the same cleanup owner immediately, not wait for the normal deadline.
    let methods = timeout(Duration::from_secs(2), peer).await???;
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "environment/info",
            "fs/createDirectory",
            "process/start",
            "process/read",
            "process/terminate",
        ],
        "actual registered remote capture terminates once without file publication or validation"
    );
    assert!(
        !directory.path().join(SNAPSHOT_DIR).exists(),
        "cancelled remote output cannot appear as a local snapshot"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SnapshotProcessEvent {
    Queued,
    Spawned(Option<u32>),
    Resumed(u32),
}

pub(super) type SnapshotProcessEvents = Arc<std::sync::Mutex<Vec<SnapshotProcessEvent>>>;

std::thread_local! {
    // Taken at the normal entry and explicitly carried to its actual worker.
    // This observer never substitutes the command, clock or process operation.
    static SNAPSHOT_PROCESS_OBSERVER: std::cell::RefCell<Option<SnapshotProcessEvents>> = const {
        std::cell::RefCell::new(None)
    };
}

pub(super) fn take_snapshot_process_observer() -> Option<SnapshotProcessEvents> {
    SNAPSHOT_PROCESS_OBSERVER.with(|observer| observer.borrow_mut().take())
}

pub(super) fn record_snapshot_process_event(
    observer: &Option<SnapshotProcessEvents>,
    event: SnapshotProcessEvent,
) {
    if let Some(observer) = observer {
        observer.lock().expect("snapshot observer lock").push(event);
    }
}

fn observe_next_snapshot_process() -> SnapshotProcessEvents {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    SNAPSHOT_PROCESS_OBSERVER.with(|observer| {
        assert!(observer.borrow_mut().replace(Arc::clone(&events)).is_none());
    });
    events
}

fn snapshot_deadline_test_shell() -> Shell {
    Shell {
        shell_type: ShellType::Cmd,
        shell_path: std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cmd.exe")),
    }
}

pub(super) type SnapshotSpawnGate = Option<(
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
)>;

std::thread_local! {
    static SNAPSHOT_SPAWN_GATE: std::cell::RefCell<SnapshotSpawnGate> = const {
        std::cell::RefCell::new(None)
    };
}

pub(super) fn take_snapshot_spawn_gate() -> SnapshotSpawnGate {
    SNAPSHOT_SPAWN_GATE.with(|gate| gate.borrow_mut().take())
}

pub(super) async fn wait_for_snapshot_spawn_gate(gate: SnapshotSpawnGate) {
    if let Some((entered, release)) = gate {
        let _ = entered.send(());
        release.await.expect("release observed native admission");
    }
}

fn observe_next_snapshot_admission() -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    SNAPSHOT_SPAWN_GATE.with(|gate| {
        assert!(
            gate.borrow_mut()
                .replace((entered_tx, release_rx))
                .is_none()
        );
    });
    (entered_rx, release_tx)
}

struct ReleaseSnapshotWorker(Option<std::sync::mpsc::Sender<()>>);

impl Drop for ReleaseSnapshotWorker {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn occupy_snapshot_worker() -> (ReleaseSnapshotWorker, tokio::task::JoinHandle<()>) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || {
        let _ = entered_tx.send(());
        release_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("release actual snapshot worker");
    });
    entered_rx.await.expect("actual worker started");
    (ReleaseSnapshotWorker(Some(release_tx)), worker)
}

#[test]
fn windows_snapshot_queued_spawn_obeys_original_deadline() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let shell = snapshot_deadline_test_shell();
        let dir = tempdir()?;
        let cwd = dir.path().abs();
        let environment = current_environment();
        let forbidden = dir.path().join("forbidden.txt");
        for budget in [Duration::ZERO, Duration::from_millis(150)] {
            let events = observe_next_snapshot_process();
            let admission = (!budget.is_zero()).then(observe_next_snapshot_admission);
            let mut snapshot = Box::pin(run_script_with_timeout(
                &shell,
                "echo forbidden>forbidden.txt",
                budget,
                false,
                &cwd,
                &environment,
            ));
            let worker = if let Some((entered, release_spawn)) = admission {
                // Drive the actual asynchronous reservation, then gate only the
                // later native spawn phase on the sole blocking worker.
                tokio::select! {
                    entered = entered => entered.expect("native reservation completed"),
                    result = &mut snapshot => panic!("snapshot completed before reservation observation: {result:?}"),
                }
                let worker = occupy_snapshot_worker().await;
                release_spawn.send(()).expect("resume observed snapshot");
                Some(worker)
            } else {
                None
            };
            let error = snapshot.await.expect_err("expired snapshot cannot report success");
            assert!(
                error.to_string() == format!("Snapshot command timed out for {}", shell.name())
                    || error.chain().any(|cause| {
                        cause.downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == ErrorKind::TimedOut)
                    }),
                "expected the original budget to expire: {error:#}"
            );
            if !budget.is_zero() {
                assert_eq!(*events.lock().unwrap(), vec![SnapshotProcessEvent::Queued],
                    "test must reach native spawn queue before expiration");
            }
            if let Some((release, worker)) = worker {
                drop(release);
                worker.await?;
            }
            // A later job on the sole worker proves any expired spawn closure
            // has drained before checking actual native creation and effects.
            tokio::task::spawn_blocking(|| ()).await?;
            assert!(events.lock().unwrap().iter().all(|event| *event == SnapshotProcessEvent::Queued),
                "no native child may be created or resumed after queued expiry");
            assert!(!forbidden.exists());
        }
        let events = observe_next_snapshot_process();
        let output = run_script_with_timeout(
            &shell,
            "echo snapshot-ran>healthy.txt & echo snapshot-ok",
            Duration::from_secs(3),
            false,
            &cwd,
            &environment,
        ).await?;
        assert_eq!(output.trim(), "snapshot-ok");
        assert_eq!(std::fs::read_to_string(dir.path().join("healthy.txt"))?.trim(), "snapshot-ran");
        let events = events.lock().unwrap();
        assert!(matches!(events.as_slice(), [SnapshotProcessEvent::Queued,
            SnapshotProcessEvent::Spawned(Some(created)), SnapshotProcessEvent::Resumed(resumed)] if created == resumed));
        Ok(())
    })
}

#[test]
fn windows_snapshot_expired_ready_child_returns_before_owned_cleanup() -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };

    struct ObservedChild(OwnedHandle);
    impl Drop for ObservedChild {
        fn drop(&mut self) {
            // SAFETY: this owned process handle stays valid during observation
            // and provides panic cleanup if a regression leaves the child alive.
            unsafe {
                if WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_TIMEOUT {
                    TerminateProcess(self.0.as_raw_handle(), 1);
                    WaitForSingleObject(self.0.as_raw_handle(), 5_000);
                }
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let shell = snapshot_deadline_test_shell();
        let dir = tempdir()?;
        let cwd = dir.path().abs();
        let environment = current_environment();
        let events = observe_next_snapshot_process();
        let (entered, release_spawn) = observe_next_snapshot_admission();
        let mut snapshot = Box::pin(run_script_with_timeout(
            &shell, "echo forbidden>forbidden.txt", Duration::from_secs(2), false, &cwd, &environment,
        ));
        tokio::select! {
            entered = entered => entered.expect("actual native reservation completed"),
            result = &mut snapshot => panic!("snapshot completed before reservation observation: {result:?}"),
        }
        let (release, worker) = occupy_snapshot_worker().await;
        release_spawn.send(()).expect("resume observed snapshot");
        assert!(std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(snapshot.as_mut(), cx))
        }).await.is_pending());
        assert_eq!(*events.lock().unwrap(), vec![SnapshotProcessEvent::Queued]);
        // The normal caller is now parked at the real queued creation; release
        // that worker without driving the caller until its deadline expires.
        drop(release);
        worker.await?;
        timeout(Duration::from_secs(2), async {
            while !events.lock().unwrap().iter().any(|event| matches!(event, SnapshotProcessEvent::Spawned(Some(_)))) {
                tokio::task::yield_now().await;
            }
        }).await.expect("real suspended child creation must be observed");
        tokio::task::spawn_blocking(|| ()).await?;
        let pid = events.lock().unwrap().iter().find_map(|event| match event {
            SnapshotProcessEvent::Spawned(pid) => *pid,
            _ => None,
        }).expect("actual native child id");
        // SAFETY: the observed suspended child is alive. The returned process
        // handle is independently owned and used for exit proof/panic cleanup.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        assert!(!raw.is_null(), "open snapshot child: {}", std::io::Error::last_os_error());
        let observed = ObservedChild(unsafe { OwnedHandle::from_raw_handle(raw) });
        let (release, worker) = occupy_snapshot_worker().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::time::resume();
        assert!(!dir.path().join("forbidden.txt").exists());
        let result = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(snapshot.as_mut(), cx))
        }).await;
        let std::task::Poll::Ready(result) = result else {
            panic!("expired caller must return without waiting for the blocked cleanup worker");
        };
        let error = result.expect_err("expired ready child must not run");
        assert!(format!("{error:#}").contains("timed out"));
        assert!(!dir.path().join("forbidden.txt").exists());
        assert!(matches!(events.lock().unwrap().as_slice(), [SnapshotProcessEvent::Queued, SnapshotProcessEvent::Spawned(Some(_))]));
        // SAFETY: the independent handle remains valid throughout both checks.
        assert_eq!(unsafe { WaitForSingleObject(observed.0.as_raw_handle(), 0) }, WAIT_TIMEOUT,
            "the blocked cleanup owner must retain the actual suspended child after caller return");
        drop(release);
        worker.await?;
        tokio::task::spawn_blocking(|| ()).await?;
        assert_eq!(unsafe { WaitForSingleObject(observed.0.as_raw_handle(), 0) }, WAIT_OBJECT_0);
        assert!(!dir.path().join("forbidden.txt").exists());
        Ok(())
    })
}
