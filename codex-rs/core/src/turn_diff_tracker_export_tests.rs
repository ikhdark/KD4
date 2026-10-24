use super::*;
use codex_exec_server::LOCAL_FS;
use codex_utils_path_uri::PathUri;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;

async fn apply(root: &Path, patch: &str) -> AppliedPatchDelta {
    codex_apply_patch::apply_patch(patch, &PathUri::from_host_native_path(root).unwrap(),
        &mut Vec::new(), &mut Vec::new(), LOCAL_FS.as_ref(), None).await.unwrap()
}

fn git_apply(root: &Path, patch: &str, reverse: bool) {
    let mut command = Command::new("git");
    command.current_dir(root).args(["-c", "core.autocrlf=false", "apply"]);
    if reverse { command.arg("--reverse"); }
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(patch.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}\n{patch}", String::from_utf8_lossy(&output.stderr));
}

#[tokio::test]
async fn export_roots_do_not_include_ui_environment_labels() {
    let alpha = tempfile::tempdir().unwrap();
    let beta = tempfile::tempdir().unwrap();
    let mut tracker = TurnDiffTracker::with_environment_display_roots([
        ("alpha".into(), alpha.path().into()), ("beta".into(), beta.path().into()),
    ]);
    tracker.track_delta("alpha", &apply(alpha.path(), "*** Begin Patch\n*** Add File: owned\n+alpha\n*** End Patch").await);
    let (environment, patch) = tracker.exact_export_diff(None).unwrap().unwrap();
    assert_eq!(environment, "alpha");
    assert!(patch.contains("diff --git a/owned b/owned"));
    assert!(!patch.contains("alpha/owned"));
    let replay = tempfile::tempdir().unwrap();
    git_apply(replay.path(), &patch, false);
    assert_eq!(std::fs::read_to_string(replay.path().join("owned")).unwrap(), "alpha\n");
    tracker.track_delta("beta", &apply(beta.path(), "*** Begin Patch\n*** Add File: owned\n+beta\n*** End Patch").await);
    assert!(tracker.exact_export_diff(None).is_err());
    assert!(tracker.exact_export_diff(Some("alpha")).unwrap().unwrap().1.contains("+alpha"));
    assert!(tracker.exact_export_diff(Some("beta")).unwrap().unwrap().1.contains("+beta"));
}

#[test]
fn executor_spelling_is_not_folded_using_the_host_case_or_separator_policy() {
    assert_ne!(TrackedPath::new("remote", Path::new("Owner.rs")), TrackedPath::new("remote", Path::new("owner.rs")));
    assert_ne!(TrackedPath::new("remote", Path::new("dir\\file")), TrackedPath::new("remote", Path::new("dir/file")));
}

#[cfg(unix)]
#[tokio::test]
async fn quoted_posix_paths_replay_only_the_owned_endpoint() {
    for name in ["dir\\file", "tab\tname", "quote\"name", "unicode-ñ"] {
        let root = tempfile::tempdir().unwrap();
        let replay = tempfile::tempdir().unwrap();
        for dir in [root.path(), replay.path()] {
            std::fs::create_dir(dir.join("dir")).unwrap();
            std::fs::write(dir.join("dir/file"), "before\n").unwrap();
            std::fs::write(dir.join(name), "before\n").unwrap();
        }
        let mut tracker = TurnDiffTracker::with_environment_display_roots([("local".into(), root.path().into())]);
        let delta = apply(root.path(), &format!("*** Begin Patch\n*** Update File: {name}\n@@\n-before\n+after\n*** End Patch")).await;
        tracker.track_delta("local", &delta);
        let patch = tracker.exact_export_diff(None).unwrap().unwrap().1;
        git_apply(replay.path(), &patch, false);
        assert_eq!(std::fs::read_to_string(replay.path().join(name)).unwrap(), "after\n");
        assert_eq!(std::fs::read_to_string(replay.path().join("dir/file")).unwrap(), "before\n");
        git_apply(replay.path(), &patch, true);
        assert_eq!(std::fs::read_to_string(replay.path().join(name)).unwrap(), "before\n");
    }
}

#[tokio::test]
async fn lf_framing_preserves_bare_cr_crlf_and_unterminated_content() {
    for before in ["one\rtwo\r", "one\rtwo\nthree\r", "one\r\ntwo\r\n", "last line", "one\ntwo\n"] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("source"), before).unwrap();
        let mut tracker = TurnDiffTracker::with_environment_display_roots([("local".into(), root.path().into())]);
        tracker.track_delta("local", &apply(root.path(), "*** Begin Patch\n*** Delete File: source\n*** End Patch").await);
        let patch = tracker.exact_export_diff(None).unwrap().unwrap().1;
        git_apply(root.path(), &patch, true);
        assert_eq!(std::fs::read(root.path().join("source")).unwrap(), before.as_bytes());
        git_apply(root.path(), &patch, false);
        assert!(!root.path().join("source").exists());
    }
}



#[test]
fn command_path_prefixes_require_an_executor_separator_boundary() {
    let root = TrackedPath::new("remote", Path::new("/work/owned"));
    assert!(TrackedPath::new("remote", Path::new("/work/owned/file")).is_within(&root, false));
    assert!(!TrackedPath::new("remote", Path::new("/work/owned-other")).is_within(&root, false));
    assert!(!TrackedPath::new("remote", Path::new("/work/owned\\file")).is_within(&root, false));
}

#[tokio::test]
async fn machine_export_refuses_an_endpoint_outside_its_replay_root() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut tracker = TurnDiffTracker::with_environment_display_roots([("local".into(), workspace.path().into())]);
    tracker.track_delta("local", &apply(outside.path(), "*** Begin Patch\n*** Add File: foreign\n+outside\n*** End Patch").await);
    assert!(tracker.exact_export_diff(None).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn deletion_mode_is_not_reconstructed_from_a_recreated_endpoint() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("executable");
    std::fs::write(&path, "owned\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let delta = apply(root.path(), "*** Begin Patch\n*** Delete File: executable\n*** End Patch").await;
    std::fs::write(&path, "foreign\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut tracker = TurnDiffTracker::with_environment_display_roots([("local".into(), root.path().into())]);
    tracker.track_delta("local", &delta);
    let patch = tracker.exact_export_diff(None).unwrap().unwrap().1;
    assert!(patch.contains("deleted file mode 100755"));
    let replay = tempfile::tempdir().unwrap();
    git_apply(replay.path(), &patch, true);
    assert_eq!(std::fs::read_to_string(replay.path().join("executable")).unwrap(), "owned\n");
    assert_ne!(std::fs::metadata(replay.path().join("executable")).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "foreign\n");
}


