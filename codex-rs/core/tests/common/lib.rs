#![allow(clippy::expect_used)]

use codex_test_binary_support::Arg0PathEntryGuard;
use codex_utils_cargo_bin::CargoBinError;
use ctor::ctor;
use tempfile::TempDir;

use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::test_support::CloudConfigBundleFixture;
use codex_core::CodexThread;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
pub use codex_core::test_support::TestCodexResponsesRequestKind;
pub use codex_core::test_support::responses_metadata;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
pub use codex_utils_absolute_path::test_support::PathBufExt;
pub use codex_utils_absolute_path::test_support::PathExt;
use codex_utils_path_uri::PathUri;
use regex_lite::Regex;
use std::path::Path;
use std::path::PathBuf;

pub mod apps_test_server;
pub mod context_snapshot;
pub mod hooks;
pub mod process;
pub mod responses;
pub mod streaming_sse;
pub mod test_codex;
pub mod test_codex_exec;
pub mod tracing;

#[ctor(unsafe)]
fn enable_deterministic_unified_exec_process_ids_for_tests() {
    codex_core::test_support::set_thread_manager_test_mode(/*enabled*/ true);
    codex_core::test_support::set_deterministic_process_ids(/*enabled*/ true);
}

// The one arg0 dispatch for every test binary linking this crate: exec-server
// local runtimes re-execute the test binary as their helper, and ordinary test
// processes get apply_patch aliases outside the developer's Codex home.
#[ctor(unsafe)]
static TEST_BINARY_DISPATCH: Option<Arg0PathEntryGuard> =
    codex_test_binary_support::configure_test_binary_dispatch("codex-core-tests");

#[ctor(unsafe)]
fn configure_insta_workspace_root_for_snapshot_tests() {
    if std::env::var_os("INSTA_WORKSPACE_ROOT").is_some() {
        return;
    }

    let workspace_root = codex_utils_cargo_bin::repo_root()
        .ok()
        .map(|root| root.join("codex-rs"));

    if let Some(workspace_root) = workspace_root
        && let Ok(workspace_root) = workspace_root.canonicalize()
    {
        // Safety: this ctor runs at process startup before test threads begin.
        unsafe {
            std::env::set_var("INSTA_WORKSPACE_ROOT", workspace_root);
        }
    }
}

#[ctor(unsafe)]
fn isolate_state_db_from_developer_environment() {
    // A developer shell may point the state DB at a live Codex home. Test configs
    // must derive it from their temporary CODEX_HOME; children may still set it.
    // Safety: this ctor runs at process startup before test threads begin.
    unsafe {
        std::env::remove_var("CODEX_SQLITE_HOME");
    }
}

#[track_caller]
pub fn assert_regex_match<'s>(pattern: &str, actual: &'s str) -> regex_lite::Captures<'s> {
    let regex = Regex::new(pattern).expect("failed to compile regex");
    regex
        .captures(actual)
        .expect("regex did not match actual value")
}

pub fn test_path_buf_with_windows(unix_path: &str, windows_path: Option<&str>) -> PathBuf {
    if let Some(windows) = windows_path {
        return PathBuf::from(windows);
    }
    let mut path = PathBuf::from(r"C:\");
    path.extend(
        unix_path
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty()),
    );
    path
}

pub fn test_path_buf(unix_path: &str) -> PathBuf {
    test_path_buf_with_windows(unix_path, /*windows_path*/ None)
}

pub fn test_absolute_path_with_windows(
    unix_path: &str,
    windows_path: Option<&str>,
) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(test_path_buf_with_windows(unix_path, windows_path))
        .expect("test path should be absolute")
}

pub fn test_absolute_path(unix_path: &str) -> AbsolutePathBuf {
    test_absolute_path_with_windows(unix_path, /*windows_path*/ None)
}

#[allow(clippy::expect_used)]
pub fn create_directory_symlink(source: &Path, link: &Path) {
    // Running this test locally may require Windows Developer Mode or an elevated process.
    std::os::windows::fs::symlink_dir(source, link)
        .expect("create directory symlink; enable Developer Mode or run the test elevated");
}

pub trait TempDirExt {
    fn abs(&self) -> AbsolutePathBuf;
}

impl TempDirExt for TempDir {
    fn abs(&self) -> AbsolutePathBuf {
        self.path().abs()
    }
}

pub fn test_tmp_path() -> AbsolutePathBuf {
    test_absolute_path_with_windows("/tmp", Some(r"C:\Users\codex\AppData\Local\Temp"))
}

pub fn test_tmp_path_buf() -> PathBuf {
    test_tmp_path().into_path_buf()
}

pub fn workspace_write_excluding_tmp() -> PermissionProfile {
    PermissionProfile::workspace_write_with(
        &[],
        NetworkSandboxPolicy::Restricted,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    )
}

pub fn requested_directory_write_permissions(path: &Path) -> RequestPermissionProfile {
    RequestPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![PathUri::from_abs_path(
                &AbsolutePathBuf::try_from(path).expect("absolute path"),
            )]),
        )),
        ..RequestPermissionProfile::default()
    }
}

pub fn normalized_directory_write_permissions(
    path: &Path,
) -> anyhow::Result<RequestPermissionProfile> {
    Ok(RequestPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![PathUri::from_abs_path(&AbsolutePathBuf::try_from(
                path.canonicalize()?,
            )?)]),
        )),
        ..RequestPermissionProfile::default()
    })
}

/// Returns a default `Config` whose on-disk state is confined to the provided
/// temporary directory. Using a per-test directory keeps tests hermetic and
/// avoids clobbering a developer’s real `~/.codex`.
pub async fn load_default_config_for_test(codex_home: &TempDir) -> Config {
    load_default_config_for_test_with_cloud_config_bundle(
        codex_home,
        CloudConfigBundleLoader::default(),
    )
    .await
}

/// Returns a default `Config` with test-provided cloud bundle requirements applied.
/// during config construction.
pub async fn load_default_config_for_test_with_cloud_config_bundle(
    codex_home: &TempDir,
    cloud_config_bundle: CloudConfigBundleLoader,
) -> Config {
    let mut config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(default_test_overrides(codex_home.path()))
        .cloud_config_bundle(cloud_config_bundle)
        .build()
        .await
        .expect("defaults for test should always succeed");
    // Do not let a developer-level CODEX_SQLITE_HOME override make otherwise
    // hermetic integration tests share one state database.
    config.sqlite_home = codex_home.path().join("sqlite");
    config
}

pub fn managed_network_requirements_loader() -> CloudConfigBundleLoader {
    CloudConfigBundleFixture::loader_with_enterprise_requirement(
        r#"
[experimental_network]
enabled = true
allow_local_binding = true
"#,
    )
}

fn default_test_overrides(codex_home: &Path) -> ConfigOverrides {
    ConfigOverrides {
        cwd: Some(codex_home.to_path_buf()),
        ..ConfigOverrides::default()
    }
}

pub async fn wait_for_event<F>(
    codex: &CodexThread,
    predicate: F,
) -> codex_protocol::protocol::EventMsg
where
    F: FnMut(&codex_protocol::protocol::EventMsg) -> bool,
{
    use tokio::time::Duration;
    wait_for_event_with_timeout(codex, predicate, Duration::from_secs(1)).await
}

/// Waits for a configured MCP server to finish startup and requires it to be ready.
pub async fn wait_for_mcp_server(codex: &CodexThread, server_name: &str) -> anyhow::Result<()> {
    use codex_protocol::protocol::EventMsg;

    // Wait for the startup summary regardless of outcome, then interpret the
    // requested server's ready, failed, or cancelled entry below.
    let summary = loop {
        let event = codex
            .next_event()
            .await
            .expect("stream ended unexpectedly while waiting for MCP startup");
        if let EventMsg::McpStartupComplete(summary) = event.msg {
            break summary;
        }
    };
    if let Some(failure) = summary
        .failed
        .iter()
        .find(|failure| failure.server == server_name)
    {
        let error = &failure.error;
        anyhow::bail!("MCP server {server_name} failed to start: {error}");
    }
    if summary.cancelled.iter().any(|server| server == server_name) {
        anyhow::bail!("MCP server {server_name} startup was cancelled");
    }
    assert!(
        summary.ready.iter().any(|server| server == server_name),
        "expected MCP server {server_name} to be ready; startup summary: {summary:?}"
    );
    Ok(())
}

pub async fn submit_thread_settings(
    codex: &CodexThread,
    thread_settings: codex_protocol::protocol::ThreadSettingsOverrides,
) -> anyhow::Result<()> {
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::Op;
    use tokio::time::Duration;
    use tokio::time::Instant;
    use tokio::time::timeout_at;

    let submission_id = codex.submit(Op::ThreadSettings { thread_settings }).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ev = timeout_at(deadline, codex.next_event())
            .await
            .expect("timeout waiting for thread settings update")
            .expect("stream ended unexpectedly");
        if ev.id == submission_id {
            match ev.msg {
                EventMsg::ThreadSettingsApplied(_) => return Ok(()),
                EventMsg::Error(err) => panic!("thread settings update failed: {}", err.message),
                other => panic!("unexpected thread settings update event: {other:?}"),
            }
        }
    }
}

pub async fn wait_for_event_match<T, F>(codex: &CodexThread, matcher: F) -> T
where
    F: Fn(&codex_protocol::protocol::EventMsg) -> Option<T>,
{
    wait_for_event_match_with_timeout(codex, matcher, INTEGRATION_EVENT_TIMEOUT).await
}

const INTEGRATION_EVENT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

pub async fn wait_for_event_match_with_timeout<T, F>(
    codex: &CodexThread,
    matcher: F,
    wait_time: tokio::time::Duration,
) -> T
where
    F: Fn(&codex_protocol::protocol::EventMsg) -> Option<T>,
{
    let ev = wait_for_event_with_timeout(codex, |ev| matcher(ev).is_some(), wait_time).await;
    matcher(&ev).expect("EventMsg should match matcher predicate")
}

pub async fn wait_for_event_with_timeout<F>(
    codex: &CodexThread,
    mut predicate: F,
    wait_time: tokio::time::Duration,
) -> codex_protocol::protocol::EventMsg
where
    F: FnMut(&codex_protocol::protocol::EventMsg) -> bool,
{
    wait_for_event_envelope_with_timeout(codex, |ev| predicate(&ev.msg), wait_time)
        .await
        .msg
}

/// Preserve operation IDs for scenarios with multiple submissions or objects.
pub async fn wait_for_event_envelope_with_timeout<F>(
    codex: &CodexThread,
    predicate: F,
    wait_time: tokio::time::Duration,
) -> codex_protocol::protocol::Event
where
    F: FnMut(&codex_protocol::protocol::Event) -> bool,
{
    event_before_deadline(
        wait_time,
        || async { codex.next_event().await.expect("stream ended unexpectedly") },
        predicate,
    )
    .await
    .expect("timeout waiting for event")
}

async fn event_before_deadline<T, N, E, P>(
    wait_time: tokio::time::Duration,
    mut next_event: N,
    mut predicate: P,
) -> Result<T, tokio::time::error::Elapsed>
where
    N: FnMut() -> E,
    E: std::future::Future<Output = T>,
    P: FnMut(&T) -> bool,
{
    let deadline = tokio::time::Instant::now() + wait_time;
    tokio::time::timeout_at(deadline, async {
        loop {
            let event = next_event().await;
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
}

pub fn sandbox_network_env_var() -> &'static str {
    codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
}

pub fn format_with_current_shell(command: &str) -> Vec<String> {
    codex_core::shell::default_user_shell()
        .derive_exec_args(command, /*use_login_shell*/ true)
        .expect("default Windows shell must be executable")
}

pub fn format_with_current_shell_display(command: &str) -> String {
    let args = format_with_current_shell(command);
    codex_app_server_protocol::command_display_string(&args)
}

pub fn format_with_current_shell_non_login(command: &str) -> Vec<String> {
    codex_core::shell::default_user_shell()
        .derive_exec_args(command, /*use_login_shell*/ false)
        .expect("default Windows shell must be executable")
}

pub fn format_with_current_shell_display_non_login(command: &str) -> String {
    let args = format_with_current_shell_non_login(command);
    codex_app_server_protocol::command_display_string(&args)
}

/// Resolves a helper binary the caller requires. Lookup failure is returned so
/// the caller propagates it; a required helper must never turn into a silent
/// pass. The owning test target declares the helper in
/// `codex-rs/.config/kd4-rust-tests.toml`, which builds it before the run.
fn required_helper_bin(name: &str) -> Result<String, CargoBinError> {
    codex_utils_cargo_bin::cargo_bin(name).map(|path| path.to_string_lossy().to_string())
}

pub fn stdio_server_bin() -> Result<String, CargoBinError> {
    required_helper_bin("test_stdio_server")
}

pub mod fs_wait {
    use anyhow::Result;
    use anyhow::anyhow;
    use notify::RecursiveMode;
    use notify::Watcher;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;
    use std::time::Instant;
    use tokio::task;
    use walkdir::WalkDir;

    pub async fn wait_for_path_exists(
        path: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<PathBuf> {
        let path = path.into();
        task::spawn_blocking(move || wait_for_path_exists_blocking(path, timeout)).await?
    }

    pub async fn wait_for_matching_file(
        root: impl Into<PathBuf>,
        timeout: Duration,
        predicate: impl FnMut(&Path) -> bool + Send + 'static,
    ) -> Result<PathBuf> {
        let root = root.into();
        task::spawn_blocking(move || {
            let mut predicate = predicate;
            blocking_find_matching_file(root, timeout, &mut predicate)
        })
        .await?
    }

    fn wait_for_path_exists_blocking(path: PathBuf, timeout: Duration) -> Result<PathBuf> {
        if path.exists() {
            return Ok(path);
        }

        let watch_root = nearest_existing_ancestor(&path);
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher.watch(&watch_root, RecursiveMode::Recursive)?;

        let deadline = Instant::now() + timeout;
        loop {
            if path.exists() {
                return Ok(path);
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            match rx.recv_timeout(remaining) {
                Ok(Ok(_event)) => {
                    if path.exists() {
                        return Ok(path);
                    }
                }
                Ok(Err(err)) => return Err(err.into()),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if path.exists() {
            Ok(path)
        } else {
            Err(anyhow!("timed out waiting for {path:?}"))
        }
    }

    fn blocking_find_matching_file(
        root: PathBuf,
        timeout: Duration,
        predicate: &mut impl FnMut(&Path) -> bool,
    ) -> Result<PathBuf> {
        let root = wait_for_path_exists_blocking(root, timeout)?;

        if let Some(found) = scan_for_match(&root, predicate) {
            return Ok(found);
        }

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;

        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(Ok(_event)) => {
                    if let Some(found) = scan_for_match(&root, predicate) {
                        return Ok(found);
                    }
                }
                Ok(Err(err)) => return Err(err.into()),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if let Some(found) = scan_for_match(&root, predicate) {
            Ok(found)
        } else {
            Err(anyhow!("timed out waiting for matching file in {root:?}"))
        }
    }

    fn scan_for_match(root: &Path, predicate: &mut impl FnMut(&Path) -> bool) -> Option<PathBuf> {
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if !entry.file_type().is_file() {
                continue;
            }
            if predicate(path) {
                return Some(path.to_path_buf());
            }
        }
        None
    }

    fn nearest_existing_ancestor(path: &Path) -> PathBuf {
        let mut current = path;
        loop {
            if current.exists() {
                return current.to_path_buf();
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => return PathBuf::from("."),
            }
        }
    }
}

/// Fail when `CODEX_SANDBOX_NETWORK_DISABLED` is present: the test requires
/// network access and cannot verify its behavior in that sandbox.
///
/// This checks the sandbox marker, not connectivity to a remote endpoint.
#[macro_export]
macro_rules! require_network {
    () => {{
        if ::std::env::var($crate::sandbox_network_env_var()).is_ok() {
            panic!("Behavior unverified: required network access is unavailable in this sandbox.");
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_prerequisite_fails_instead_of_skipping() {
        const CHILD: &str = "CODEX_TEST_NETWORK_PREREQUISITE_CHILD";
        const REACHED: &str = "network prerequisite passed";
        if std::env::var_os(CHILD).is_some() {
            require_network!();
            println!("{REACHED}");
            return;
        }

        for disabled in [false, true] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "tests::network_prerequisite_fails_instead_of_skipping",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env_remove(sandbox_network_env_var());
            if disabled {
                child.env(sandbox_network_env_var(), "1");
            }
            let output = child
                .output()
                .expect("run prerequisite in isolated process");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), !disabled, "{stdout}\n{stderr}");
            assert_eq!(stdout.contains(REACHED), !disabled, "{stdout}\n{stderr}");
            if disabled {
                assert!(stderr.contains("Behavior unverified: required network access"));
            }
        }
    }

    #[test]
    fn host_path_fixture_uses_host_convention() {
        let path = test_path_buf_with_windows("/tmp/kd4-test", Some(r"C:\tmp\kd4-test"));

        assert_eq!(path, PathBuf::from(r"C:\tmp\kd4-test"));
    }

    #[tokio::test]
    async fn default_config_keeps_multi_agent_v2_enabled() {
        let codex_home = tempfile::tempdir().expect("create test Codex home");
        let config = load_default_config_for_test(&codex_home).await;

        assert!(
            config
                .features
                .enabled(codex_features::Feature::MultiAgentV2)
        );
    }

    #[tokio::test]
    async fn event_waiter_honors_requested_deadline() {
        use codex_protocol::protocol::Event;
        use codex_protocol::protocol::EventMsg;
        use tokio::time::Duration;

        // Only the event source is substituted; the production wait/match loop runs.
        let mut observed = 0;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            event_before_deadline(
                Duration::from_millis(20),
                || async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    Event {
                        id: "unrelated-operation".into(),
                        msg: EventMsg::ShutdownComplete,
                    }
                },
                |event| {
                    observed += 1;
                    event.id == "submitted-operation"
                        && matches!(event.msg, EventMsg::ShutdownComplete)
                },
            ),
        )
        .await
        .expect("unrelated events must not renew the total deadline");
        assert!(result.is_err());
        assert!(
            observed > 0,
            "the waiter must reject an event of the right kind from the wrong operation"
        );
    }
}
