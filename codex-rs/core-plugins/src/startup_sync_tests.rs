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
