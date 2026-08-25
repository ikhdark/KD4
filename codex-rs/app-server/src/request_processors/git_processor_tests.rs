use super::git_diff_to_remote_response;
use crate::error_code::INVALID_REQUEST_ERROR_CODE;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::GitDiffToRemoteParams;
use codex_app_server_protocol::GitSha;
use std::path::Path;
use std::process::Command;

#[tokio::test]
async fn git_diff_to_remote_response_maps_git_result_to_protocol_payload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let remote = temp.path().join("remote.git");
    std::fs::create_dir_all(&repo).expect("create repository directory");
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "core.autocrlf", "false"]);
    run_git(&repo, &["config", "user.email", "test@example.com"]);
    run_git(&repo, &["config", "user.name", "Test User"]);
    std::fs::write(repo.join("tracked.txt"), "base\n").expect("write base file");
    run_git(&repo, &["add", "tracked.txt"]);
    run_git(&repo, &["commit", "-m", "base"]);
    let branch = run_git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let base_sha = run_git(&repo, &["rev-parse", "HEAD"]);
    run_git(
        temp.path(),
        &[
            "init",
            "--bare",
            remote.to_str().expect("utf-8 remote path"),
        ],
    );
    run_git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("utf-8 remote path"),
        ],
    );
    run_git(&repo, &["push", "-u", "origin", &branch]);
    std::fs::write(repo.join("tracked.txt"), "base\nlocal\n").expect("write local change");

    let payload = git_diff_to_remote_response(GitDiffToRemoteParams { cwd: repo })
        .await
        .expect("git diff response")
        .expect("response payload");
    let ClientResponsePayload::GitDiffToRemote(response) = payload else {
        panic!("expected GitDiffToRemote response payload");
    };

    assert_eq!(response.sha, GitSha::new(&base_sha));
    assert!(response.diff.contains("tracked.txt"));
    assert!(response.diff.contains("+local"));
}

#[tokio::test]
async fn git_diff_to_remote_response_preserves_invalid_request_error() {
    let cwd = tempfile::tempdir().expect("tempdir").path().to_path_buf();

    let error = git_diff_to_remote_response(GitDiffToRemoteParams { cwd: cwd.clone() })
        .await
        .expect_err("non-repository should fail");

    assert_eq!(error.code, INVALID_REQUEST_ERROR_CODE);
    assert_eq!(
        error.message,
        format!("failed to compute git diff to remote for cwd: {cwd:?}")
    );
}

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git command");
    assert!(
        output.status.success(),
        "git command failed: {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
