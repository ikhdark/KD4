use super::*;
use crate::test_support::TEST_CURATED_PLUGIN_SHA;
use crate::test_support::write_curated_plugin_sha;
use crate::test_support::write_manifest_only_openai_curated_marketplace as write_openai_curated_marketplace;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use tempfile::tempdir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;
use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

fn test_http_clients() -> RouteAwareClientPool {
    create_client_pool_without_request_logging(
        HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault),
        ClientRouteClass::Api,
    )
}

#[cfg(unix)]
#[test]
fn plugin_git_unix_exit_poll_retains_root_identity_until_cleanup() {
    use std::os::unix::process::CommandExt;
    use std::os::unix::process::ExitStatusExt;

    for (script, exit_code, signal) in [
        ("exit 23", Some(23), None),
        ("kill -TERM $$", None, Some(libc::SIGTERM)),
    ] {
        let child = Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .spawn()
            .expect("owned root");
        let pid = child.id();
        let mut owner = GitChild {
            child,
            completed: false,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = owner.try_wait().expect("observe root exit") {
                break status;
            }
            assert!(std::time::Instant::now() < deadline, "root did not exit");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), exit_code);
        assert_eq!(status.signal(), signal);

        // Observe the kernel independently: the owner's poll must not consume
        // the exited root that pins the numeric group identity during cleanup.
        // SAFETY: siginfo_t permits zero initialization for a WNOHANG probe.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            // SAFETY: the owned child PID and writable info buffer remain valid;
            // WNOWAIT prevents this independent observation from reaping it.
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            },
            0
        );
        // SAFETY: successful waitid populated the SIGCHLD fields.
        assert_eq!(unsafe { info.si_pid() }, pid as libc::pid_t);
        drop(owner);
        // SAFETY: waitpid accepts a null status pointer; this probes that the
        // owner already reaped its exact former child and never sends a signal.
        let result =
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(result, -1, "owner cleanup must reap the root");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}

#[cfg(unix)]
#[test]
fn plugin_git_unix_runner_preserves_only_successful_background_helper() {
    for exit_code in [0, 23] {
        let tmp = tempdir().expect("helper markers");
        let ready = tmp.path().join("ready");
        let release = tmp.path().join("release");
        let survived = tmp.path().join("survived");
        // Positional arguments keep paths out of shell source. The helper is
        // bounded even if an assertion interrupts the parent before release.
        let script = r#"
            (
                printf ready > "$1"
                count=0
                while [ ! -e "$2" ] && [ "$count" -lt 500 ]; do
                    sleep 0.01
                    count=$((count + 1))
                done
                if [ -e "$2" ]; then printf survived > "$3"; fi
            ) &
            while [ ! -e "$1" ]; do sleep 0.01; done
            printf 'root output'
            printf 'root diagnostic' >&2
            exit "$4"
        "#;
        let mut command = Command::new("sh");
        command
            .args(["-c", script, "git-helper"])
            .arg(&ready)
            .arg(&release)
            .arg(&survived)
            .arg(exit_code.to_string());
        let output = run_git_command_with_timeout(
            &mut command,
            "Git Unix helper fixture",
            Duration::from_secs(5),
        )
        .expect("root output completes without waiting for helper");
        assert_eq!(output.status.code(), Some(exit_code));
        assert_eq!(output.stdout, b"root output");
        assert_eq!(output.stderr, b"root diagnostic");
        assert!(ready.exists(), "helper really started before root exit");
        std::fs::write(&release, b"release").expect("release helper");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !survived.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            survived.exists(),
            exit_code == 0,
            "failed Git roots terminate helpers; successful roots preserve them"
        );
        if exit_code == 0 {
            assert_eq!(std::fs::read(&survived).unwrap(), b"survived");
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn plugin_git_large_output_startup_sync() {
    let tmp = tempdir().expect("temp directory");
    let repo = curated_plugins_repo_path(tmp.path());
    std::fs::create_dir_all(repo.join(".git")).expect("existing checkout");
    std::fs::write(repo.join("retained"), "installed plugin").expect("installed plugin");
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let stdout_path = tmp.path().join("stdout.txt");
    let stderr_path = tmp.path().join("stderr.txt");
    std::fs::write(
        &stdout_path,
        format!("{sha}\tHEAD\n{}", "x".repeat(1024 * 1024)),
    )
    .expect("stdout fixture");
    std::fs::write(&stderr_path, "y".repeat(1024 * 1024)).expect("stderr fixture");
    let git = tmp.path().join("git.cmd");
    std::fs::write(&git, format!(
        "@echo off\r\nif \"%1\"==\"ls-remote\" goto remote\r\necho {sha}\r\nexit /b 0\r\n:remote\r\ntype \"{}\"\r\ntype \"{}\" 1>&2\r\n",
        stdout_path.display(), stderr_path.display()
    )).expect("Git subprocess fixture");

    let result = run_sync_with_transport_overrides(
        tmp.path().to_path_buf(),
        git.to_str().expect("Git path"),
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
    )
    .await
    .expect("startup Git sync must finish without falling back to HTTP");

    assert_eq!(result, sha);
    assert_eq!(
        std::fs::read_to_string(repo.join("retained")).expect("installed plugin"),
        "installed plugin"
    );
    assert!(!has_plugins_clone_dirs(tmp.path()));
}

#[cfg(windows)]
#[test]
fn plugin_git_large_output_preserves_streams_and_exit_status() {
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::Out.Write(('x' * 1048576)); [Console]::Error.Write(('y' * 1048576)); exit 23",
    ]);
    let output =
        run_git_command_with_timeout(&mut command, "Git output fixture", Duration::from_secs(10))
            .expect("large output completes");
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, vec![b'x'; 1024 * 1024]);
    assert_eq!(output.stderr, vec![b'y'; 1024 * 1024]);
}

#[cfg(windows)]
#[test]
fn plugin_git_completion_preserves_only_successful_background_helper() {
    for exit_code in [0, 23] {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile", "-NonInteractive", "-Command",
            &format!("$info = New-Object System.Diagnostics.ProcessStartInfo; $info.FileName = 'powershell.exe'; $info.Arguments = '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 60'; $info.UseShellExecute = $false; $child = [System.Diagnostics.Process]::Start($info); [Console]::Out.Write($child.Id); exit {exit_code}"),
        ]);
        let start = std::time::Instant::now();
        let output = run_git_command_with_timeout(
            &mut command,
            "Git daemon fixture",
            Duration::from_secs(10),
        )
        .expect("root finishes despite inherited output handles");
        let pid = String::from_utf8(output.stdout)
            .expect("PID output")
            .parse::<u32>()
            .expect("background PID");
        // Observe the real daemon, then explicitly clean up any ownership
        // transferred by successful completion before asserting the observation.
        let cleanup = Command::new("powershell.exe").args([
            "-NoProfile", "-NonInteractive", "-Command",
            &format!("$child = Get-Process -Id {pid} -ErrorAction SilentlyContinue; if (!$child) {{ exit 1 }}; $child.Kill(); $child.WaitForExit(); exit 0"),
        ]).output().expect("observe and clean up background helper");
        assert_eq!(output.status.code(), Some(exit_code));
        assert_eq!(
            cleanup.status.success(),
            exit_code == 0,
            "only successful Git commands may retain a daemon"
        );
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "background handles must not keep output collection open"
        );
    }
}

#[cfg(windows)]
#[test]
fn plugin_git_timeout_reaps_descendants_and_preserves_stderr() {
    let tmp = tempdir().expect("temp directory");
    let pids = tmp.path().join("pids.txt");
    let script = tmp.path().join("process-tree.ps1");
    let pid_path = pids.to_string_lossy().replace('\'', "''");
    std::fs::write(
        &script,
        format!(
            "$childInfo = New-Object System.Diagnostics.ProcessStartInfo\n\
         $childInfo.FileName = 'powershell.exe'\n\
         $childInfo.Arguments = '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 60'\n\
         $childInfo.UseShellExecute = $false\n\
         $child = [System.Diagnostics.Process]::Start($childInfo)\n\
         [System.IO.File]::WriteAllText('{pid_path}', \"$PID $($child.Id)\")\n\
         [Console]::Error.Write('waiting for Git helper')\n\
         Start-Sleep -Seconds 60\n"
        ),
    )
    .expect("process tree script");
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script);
    let start = std::time::Instant::now();
    let error =
        run_git_command_with_timeout(&mut command, "Git timeout fixture", Duration::from_secs(5))
            .expect_err("tree must time out");
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "descendant handles must not extend the timeout"
    );
    assert_eq!(
        error,
        "Git timeout fixture timed out after 5s: waiting for Git helper"
    );
    let pids = std::fs::read_to_string(&pids).expect("root and descendant started");
    let pids = pids
        .split_whitespace()
        .map(|pid| pid.parse::<u32>().expect("process id"))
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2);
    for pid in pids {
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }}; exit 0"
                ),
            ])
            .output()
            .expect("check process termination");
        assert!(
            output.status.success(),
            "Git process {pid} survived timeout"
        );
    }
}

#[test]
fn git_command_sanitizes_ambient_repository_environment() {
    let command = git_command(Path::new("git"));

    for name in REPOSITORY_LOCAL_GIT_ENVIRONMENT_VARIABLES {
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .map(|(_, value)| value),
            Some(None),
            "{name} should be removed from startup sync Git commands"
        );
    }
}

fn has_plugins_clone_dirs(codex_home: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(codex_home.join(".tmp")) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_dir()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("plugins-clone-"))
    })
}

async fn mount_github_repo_and_ref(server: &MockServer, sha: &str) {
    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"default_branch":"main"}"#))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins/git/ref/heads/main"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"object":{{"sha":"{sha}"}}}}"#)),
        )
        .mount(server)
        .await;
}

async fn mount_github_zipball(server: &MockServer, sha: &str, bytes: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/openai/plugins/zipball/{sha}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(bytes),
        )
        .mount(server)
        .await;
}

async fn mount_export_archive(server: &MockServer, bytes: Vec<u8>) -> String {
    let export_api_url = format!("{}/backend-api/plugins/export/curated", server.uri());
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/export/curated"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"download_url":"{}/files/curated-plugins.zip"}}"#,
            server.uri()
        )))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/curated-plugins.zip"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(bytes),
        )
        .mount(server)
        .await;
    export_api_url
}

async fn run_sync_with_transport_overrides(
    codex_home: PathBuf,
    git_binary: impl Into<String>,
    api_base_url: impl Into<String>,
    backup_archive_api_url: impl Into<String>,
) -> Result<String, String> {
    let git_binary = git_binary.into();
    let api_base_url = api_base_url.into();
    let backup_archive_api_url = backup_archive_api_url.into();
    let http_clients = test_http_clients();
    tokio::task::spawn_blocking(move || {
        let git_binary = PathBuf::from(git_binary);
        sync_openai_plugins_repo_with_transport_overrides(
            codex_home.as_path(),
            Some(git_binary.as_path()),
            &api_base_url,
            &backup_archive_api_url,
            &http_clients,
        )
    })
    .await
    .expect("sync task should join")
}

async fn run_sync_without_git(
    codex_home: PathBuf,
    api_base_url: impl Into<String>,
    backup_archive_api_url: impl Into<String>,
) -> Result<String, String> {
    let api_base_url = api_base_url.into();
    let backup_archive_api_url = backup_archive_api_url.into();
    let http_clients = test_http_clients();
    tokio::task::spawn_blocking(move || {
        sync_openai_plugins_repo_with_transport_overrides(
            codex_home.as_path(),
            /*git_binary*/ None,
            &api_base_url,
            &backup_archive_api_url,
            &http_clients,
        )
    })
    .await
    .expect("sync task should join")
}

async fn run_http_sync(
    codex_home: PathBuf,
    api_base_url: impl Into<String>,
) -> Result<String, String> {
    let api_base_url = api_base_url.into();
    let http_clients = test_http_clients();
    tokio::task::spawn_blocking(move || {
        sync_openai_plugins_repo_via_http_with_clients(
            codex_home.as_path(),
            &api_base_url,
            &http_clients,
        )
    })
    .await
    .expect("sync task should join")
}

fn assert_curated_gmail_repo(repo_path: &Path) {
    assert!(repo_path.join(".agents/plugins/marketplace.json").is_file());
    assert!(
        repo_path
            .join("plugins/gmail/.codex-plugin/plugin.json")
            .is_file()
    );
}

#[test]
fn curated_plugins_repo_path_uses_codex_home_tmp_dir() {
    let tmp = tempdir().expect("tempdir");
    assert_eq!(
        curated_plugins_repo_path(tmp.path()),
        tmp.path().join(".tmp/plugins")
    );
}

#[test]
fn read_curated_plugins_sha_reads_trimmed_sha_file() {
    let tmp = tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join(".tmp")).expect("create tmp");
    std::fs::write(tmp.path().join(".tmp/plugins.sha"), "abc123\n").expect("write sha");

    assert_eq!(
        read_curated_plugins_sha(tmp.path()).as_deref(),
        Some("abc123")
    );
}

#[tokio::test]
async fn sync_openai_plugins_repo_falls_back_to_http_when_git_is_unavailable() {
    let tmp = tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let sha = "0123456789abcdef0123456789abcdef01234567";

    mount_github_repo_and_ref(&server, sha).await;
    mount_github_zipball(&server, sha, curated_repo_zipball_bytes(sha)).await;

    let synced_sha = run_sync_with_transport_overrides(
        tmp.path().to_path_buf(),
        "missing-git-for-test",
        server.uri(),
        "http://127.0.0.1:9/backend-api/plugins/export/curated",
    )
    .await
    .expect("fallback sync should succeed");

    let repo_path = curated_plugins_repo_path(tmp.path());
    assert_eq!(synced_sha, sha);
    assert_curated_gmail_repo(&repo_path);
    assert_eq!(read_curated_plugins_sha(tmp.path()).as_deref(), Some(sha));
}

#[tokio::test]
async fn sync_openai_plugins_repo_uses_http_without_git_transport() {
    let tmp = tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let sha = "0123456789abcdef0123456789abcdef01234567";

    mount_github_repo_and_ref(&server, sha).await;
    mount_github_zipball(&server, sha, curated_repo_zipball_bytes(sha)).await;

    let synced_sha = run_sync_without_git(
        tmp.path().to_path_buf(),
        server.uri(),
        "http://127.0.0.1:9/backend-api/plugins/export/curated",
    )
    .await
    .expect("HTTP sync should succeed");

    assert_eq!(synced_sha, sha);
    assert_curated_gmail_repo(&curated_plugins_repo_path(tmp.path()));
}

#[cfg(windows)]
#[tokio::test]
async fn startup_sync_sha_publish_failure_preserves_existing_snapshot_and_allows_retry() {
    use std::os::windows::fs::OpenOptionsExt;

    let tmp = tempdir().expect("tempdir");
    let repo_path = curated_plugins_repo_path(tmp.path());
    std::fs::create_dir_all(repo_path.join(".agents/plugins")).expect("existing repository");
    let old_manifest = r#"{"name":"existing-marketplace","plugins":[]}"#;
    std::fs::write(
        repo_path.join(".agents/plugins/marketplace.json"),
        old_manifest,
    )
    .expect("existing manifest");
    std::fs::write(repo_path.join("retained.txt"), "existing installed plugin")
        .expect("existing plugin contents");
    let sha_path = curated_plugins_sha_path(tmp.path());
    let old_sha = "1111111111111111111111111111111111111111";
    std::fs::write(&sha_path, format!("{old_sha}\n")).expect("existing SHA");
    // A reader that does not share writes/deletes models another Windows process
    // holding the published revision open, without replacing filesystem behavior.
    let sha_reader = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&sha_path)
        .expect("lock revision publication");
    let server = MockServer::start().await;
    let new_sha = "2222222222222222222222222222222222222222";
    mount_github_repo_and_ref(&server, new_sha).await;
    mount_github_zipball(&server, new_sha, curated_repo_zipball_bytes(new_sha)).await;

    let error = run_sync_without_git(
        tmp.path().to_path_buf(),
        server.uri(),
        "http://127.0.0.1:9/backend-api/plugins/export/curated",
    )
    .await
    .expect_err("locked SHA must fail publication");
    assert!(error.contains("curated plugins sha"), "{error}");
    assert_eq!(
        read_curated_plugins_sha(tmp.path()).as_deref(),
        Some(old_sha)
    );
    assert_eq!(
        std::fs::read_to_string(repo_path.join("retained.txt")).expect("retained plugin"),
        "existing installed plugin"
    );
    assert_eq!(
        std::fs::read_to_string(repo_path.join(".agents/plugins/marketplace.json"))
            .expect("retained manifest"),
        old_manifest
    );
    assert!(!repo_path.join("plugins/gmail").exists());
    assert!(!has_plugins_clone_dirs(tmp.path()));

    drop(sha_reader);
    let revision = run_sync_without_git(
        tmp.path().to_path_buf(),
        server.uri(),
        "http://127.0.0.1:9/backend-api/plugins/export/curated",
    )
    .await
    .expect("retry after unlocking SHA");
    assert_eq!(revision, new_sha);
    assert_eq!(
        read_curated_plugins_sha(tmp.path()).as_deref(),
        Some(new_sha)
    );
    assert_curated_gmail_repo(&repo_path);
    assert!(!repo_path.join("retained.txt").exists());
    assert!(!has_plugins_clone_dirs(tmp.path()));
}

#[tokio::test]
async fn startup_sync_http_fallback_uses_configured_proxy_routes() {
    let tmp = tempdir().expect("tempdir");
    let proxy = MockServer::start().await;
    let sha = "9876543210abcdef9876543210abcdef98765432";
    mount_github_repo_and_ref(&proxy, sha).await;
    mount_github_zipball(&proxy, sha, curated_repo_zipball_bytes(sha)).await;
    let api_base_url = "http://curated-sync.test";
    for request_url in [
        format!("{api_base_url}/repos/openai/plugins"),
        format!("{api_base_url}/repos/openai/plugins/git/ref/heads/main"),
        format!("{api_base_url}/repos/openai/plugins/zipball/{sha}"),
    ] {
        codex_http_client::cache_system_proxy_route_for_test(&request_url, proxy.uri());
    }
    let http_clients = create_client_pool_without_request_logging(
        HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::RespectSystemProxy),
        ClientRouteClass::Api,
    );
    let codex_home = tmp.path().to_path_buf();
    let synced_sha = tokio::task::spawn_blocking(move || {
        sync_openai_plugins_repo_with_transport_overrides(
            &codex_home,
            /*git_binary*/ None,
            api_base_url,
            "http://127.0.0.1:9/backend-api/plugins/export/curated",
            &http_clients,
        )
    })
    .await
    .expect("sync task should join")
    .expect("startup sync should use configured proxy routes");

    assert_eq!(synced_sha, sha);
    assert_curated_gmail_repo(&curated_plugins_repo_path(tmp.path()));
}

#[tokio::test]
async fn startup_sync_sha_publish_failure_removes_unpublished_initial_snapshot() {
    for use_backup_archive in [false, true] {
        let tmp = tempdir().expect("tempdir");
        let sha_path = curated_plugins_sha_path(tmp.path());
        std::fs::create_dir_all(&sha_path).expect("block SHA publication with a directory");
        let server = MockServer::start().await;
        let sha = "3333333333333333333333333333333333333333";
        let backup_url = if use_backup_archive {
            mount_export_archive(&server, curated_repo_backup_archive_zip_bytes(sha)).await
        } else {
            mount_github_repo_and_ref(&server, sha).await;
            mount_github_zipball(&server, sha, curated_repo_zipball_bytes(sha)).await;
            "http://127.0.0.1:9/backend-api/plugins/export/curated".to_string()
        };

        let error = run_sync_without_git(tmp.path().to_path_buf(), server.uri(), &backup_url)
            .await
            .expect_err("SHA publication should fail");
        assert!(error.contains("curated plugins sha"), "{error}");
        assert!(!curated_plugins_repo_path(tmp.path()).exists());
        assert!(sha_path.is_dir());
        assert!(!has_plugins_clone_dirs(tmp.path()));

        std::fs::remove_dir(&sha_path).expect("unblock SHA publication");
        let revision = run_sync_without_git(tmp.path().to_path_buf(), server.uri(), &backup_url)
            .await
            .expect("retry initial publication");
        assert_eq!(revision, sha);
        assert_eq!(read_curated_plugins_sha(tmp.path()).as_deref(), Some(sha));
        assert_curated_gmail_repo(&curated_plugins_repo_path(tmp.path()));
        assert!(!has_plugins_clone_dirs(tmp.path()));
    }
}

#[cfg(windows)]
#[tokio::test]
async fn startup_sync_git_publishes_checkout_and_revision_together() {
    let tmp = tempdir().expect("tempdir");
    let source = tempdir().expect("Git source");
    std::fs::create_dir_all(source.path().join(".agents/plugins")).expect("source directory");
    std::fs::write(
        source.path().join(".agents/plugins/marketplace.json"),
        r#"{"name":"git-marketplace","plugins":[]}"#,
    )
    .expect("source manifest");
    let run_git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(source.path())
            .args(args)
            .output()
            .expect("run source Git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Git output")
            .trim()
            .to_string()
    };
    run_git(&["init"]);
    run_git(&["add", "."]);
    run_git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "marketplace",
    ]);
    let revision = run_git(&["rev-parse", "HEAD"]);
    let git_wrapper = tmp.path().join("git-local-source.cmd");
    std::fs::write(
        &git_wrapper,
        format!(
            "@git -c \"url.{}.insteadOf=https://github.com/openai/plugins.git\" %*\r\n",
            source.path().to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("local Git URL routing fixture");

    let actual_revision = run_sync_with_transport_overrides(
        tmp.path().to_path_buf(),
        git_wrapper.to_string_lossy(),
        "http://127.0.0.1:9",
        "http://127.0.0.1:9",
    )
    .await
    .expect("real local Git sync");
    assert_eq!(actual_revision, revision);
    assert_eq!(read_curated_plugins_sha(tmp.path()), Some(revision));
    assert_eq!(
        std::fs::read_to_string(
            curated_plugins_repo_path(tmp.path()).join(".agents/plugins/marketplace.json")
        )
        .expect("activated Git manifest"),
        r#"{"name":"git-marketplace","plugins":[]}"#
    );
    assert!(!has_plugins_clone_dirs(tmp.path()));
}

#[tokio::test]
async fn sync_openai_plugins_repo_via_http_cleans_up_staged_dir_on_extract_failure() {
    let tmp = tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let sha = "0123456789abcdef0123456789abcdef01234567";

    mount_github_repo_and_ref(&server, sha).await;
    mount_github_zipball(&server, sha, b"not a zip archive".to_vec()).await;

    let err = run_http_sync(tmp.path().to_path_buf(), server.uri())
        .await
        .expect_err("http sync should fail");

    assert!(err.contains("failed to open curated plugins zip archive"));
    assert!(!has_plugins_clone_dirs(tmp.path()));
}

#[tokio::test]
async fn sync_openai_plugins_repo_skips_archive_download_when_sha_matches() {
    let tmp = tempdir().expect("tempdir");
    let repo_path = curated_plugins_repo_path(tmp.path());
    std::fs::create_dir_all(repo_path.join(".agents/plugins")).expect("create repo");
    std::fs::write(
        repo_path.join(".agents/plugins/marketplace.json"),
        r#"{"name":"openai-curated","plugins":[]}"#,
    )
    .expect("write marketplace");
    std::fs::create_dir_all(tmp.path().join(".tmp")).expect("create tmp");
    let sha = "fedcba9876543210fedcba9876543210fedcba98";
    std::fs::write(tmp.path().join(".tmp/plugins.sha"), format!("{sha}\n")).expect("write sha");

    let server = MockServer::start().await;
    mount_github_repo_and_ref(&server, sha).await;

    run_sync_with_transport_overrides(
        tmp.path().to_path_buf(),
        "missing-git-for-test",
        server.uri(),
        "http://127.0.0.1:9/backend-api/plugins/export/curated",
    )
    .await
    .expect("sync should succeed");

    assert_eq!(read_curated_plugins_sha(tmp.path()).as_deref(), Some(sha));
    assert!(repo_path.join(".agents/plugins/marketplace.json").is_file());
}

#[tokio::test]
async fn sync_openai_plugins_repo_falls_back_to_export_archive_when_no_snapshot_exists() {
    let tmp = tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let export_sha = "1111111111111111111111111111111111111111";

    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins"))
        .respond_with(ResponseTemplate::new(500).set_body_string("github repo lookup failed"))
        .mount(&server)
        .await;
    let export_api_url =
        mount_export_archive(&server, curated_repo_backup_archive_zip_bytes(export_sha)).await;

    let synced_sha = run_sync_with_transport_overrides(
        tmp.path().to_path_buf(),
        "missing-git-for-test",
        server.uri(),
        export_api_url,
    )
    .await
    .expect("export fallback sync should succeed");

    let repo_path = curated_plugins_repo_path(tmp.path());
    assert_eq!(synced_sha, export_sha);
    assert_curated_gmail_repo(&repo_path);
    assert_eq!(
        read_curated_plugins_sha(tmp.path()).as_deref(),
        Some(export_sha)
    );
}

#[tokio::test]
async fn sync_openai_plugins_repo_skips_export_archive_when_snapshot_exists() {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    let plugin_manifest_path = curated_root.join("plugins/linear/.codex-plugin/plugin.json");
    let original_manifest =
        std::fs::read_to_string(&plugin_manifest_path).expect("read existing plugin manifest");

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins"))
        .respond_with(ResponseTemplate::new(500).set_body_string("github repo lookup failed"))
        .mount(&server)
        .await;
    let export_api_url = mount_export_archive(
        &server,
        curated_repo_backup_archive_zip_bytes("2222222222222222222222222222222222222222"),
    )
    .await;

    let err = run_sync_with_transport_overrides(
        tmp.path().to_path_buf(),
        "missing-git-for-test",
        server.uri(),
        export_api_url,
    )
    .await
    .expect_err("existing snapshot should suppress export fallback");

    assert!(err.contains("export archive fallback skipped"));
    assert_eq!(
        std::fs::read_to_string(&plugin_manifest_path).expect("read plugin manifest after sync"),
        original_manifest
    );
    assert_eq!(
        read_curated_plugins_sha(tmp.path()).as_deref(),
        Some(TEST_CURATED_PLUGIN_SHA)
    );
}

#[test]
fn read_extracted_backup_archive_git_sha_reads_head_ref_from_extracted_repo() {
    let tmp = tempdir().expect("tempdir");
    let git_dir = tmp.path().join(".git/refs/heads");
    std::fs::create_dir_all(&git_dir).expect("create git ref dir");
    std::fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
    std::fs::write(
        git_dir.join("main"),
        "3333333333333333333333333333333333333333\n",
    )
    .expect("write main ref");

    assert_eq!(
        read_extracted_backup_archive_git_sha(tmp.path())
            .expect("read extracted backup archive git sha"),
        Some("3333333333333333333333333333333333333333".to_string())
    );
}

#[test]
fn read_extracted_backup_archive_git_sha_rejects_non_refs_head_target() {
    let tmp = tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join(".git")).expect("create git dir");
    std::fs::write(tmp.path().join(".git/HEAD"), "ref: HEAD\n").expect("write HEAD");

    let err = read_extracted_backup_archive_git_sha(tmp.path())
        .expect_err("non-refs target should be rejected");

    assert!(err.contains("must stay under refs/"));
}

#[test]
fn read_extracted_backup_archive_git_sha_rejects_path_traversal_ref() {
    let tmp = tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join(".git")).expect("create git dir");
    std::fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/../../evil\n")
        .expect("write HEAD");

    let err = read_extracted_backup_archive_git_sha(tmp.path())
        .expect_err("path traversal ref should be rejected");

    assert!(err.contains("invalid path components"));
}

fn curated_repo_zipball_bytes(sha: &str) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let root = format!("openai-plugins-{sha}");
    writer
        .start_file(format!("{root}/.agents/plugins/marketplace.json"), options)
        .expect("start marketplace entry");
    writer
        .write_all(
            br#"{
  "name": "openai-curated",
  "plugins": [
    {
      "name": "gmail",
      "source": {
        "source": "local",
        "path": "./plugins/gmail"
      }
    }
  ]
}"#,
        )
        .expect("write marketplace");
    writer
        .start_file(
            format!("{root}/plugins/gmail/.codex-plugin/plugin.json"),
            options,
        )
        .expect("start plugin manifest entry");
    writer
        .write_all(br#"{"name":"gmail"}"#)
        .expect("write plugin manifest");

    writer.finish().expect("finish zip writer").into_inner()
}

fn curated_repo_backup_archive_zip_bytes(sha: &str) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default();

    writer
        .start_file("plugins/.git/HEAD", options)
        .expect("start HEAD entry");
    writer
        .write_all(b"ref: refs/heads/main\n")
        .expect("write HEAD");
    writer
        .start_file("plugins/.git/refs/heads/main", options)
        .expect("start main ref entry");
    writer
        .write_all(format!("{sha}\n").as_bytes())
        .expect("write main ref");
    writer
        .start_file("plugins/.agents/plugins/marketplace.json", options)
        .expect("start marketplace entry");
    writer
        .write_all(
            br#"{
  "name": "openai-curated",
  "plugins": [
    {
      "name": "gmail",
      "source": {
        "source": "local",
        "path": "./plugins/gmail"
      }
    }
  ]
}"#,
        )
        .expect("write marketplace");
    writer
        .start_file("plugins/plugins/gmail/.codex-plugin/plugin.json", options)
        .expect("start plugin manifest entry");
    writer
        .write_all(br#"{"name":"gmail"}"#)
        .expect("write plugin manifest");

    writer.finish().expect("finish zip writer").into_inner()
}
