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
