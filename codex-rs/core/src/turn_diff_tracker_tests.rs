use super::*;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::MaybeApplyPatchVerified;
use codex_exec_server::LOCAL_FS;
use codex_git_utils::ApplyGitRequest;
use codex_git_utils::apply_git_patch;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;
use tempfile::tempdir;

fn git_blob_sha1_hex(data: &str) -> String {
    format!("{:x}", git_blob_sha1_hex_bytes(data.as_bytes()))
}

async fn apply_verified_patch(root: &Path, patch: &str) -> AppliedPatchDelta {
    let cwd = PathUri::from_host_native_path(root).expect("absolute tempdir path");
    let argv = vec!["apply_patch".to_string(), patch.to_string()];
    match codex_apply_patch::maybe_parse_apply_patch_verified(
        &argv,
        &cwd,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    {
        MaybeApplyPatchVerified::Body(_) => {}
        other => panic!("expected verified patch action, got {other:?}"),
    }

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    codex_apply_patch::apply_patch(
        patch,
        &cwd,
        &mut stdout,
        &mut stderr,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    .expect("patch should apply")
}

fn tracker_with_root(root: &Path) -> TurnDiffTracker {
    TurnDiffTracker::with_environment_display_roots([("".to_string(), root.to_path_buf())])
}

trait TestTurnDiffTrackerExt {
    fn record_exec_command_end(&mut self, command: &[String], exit_code: i32, timed_out: bool);
}

impl TestTurnDiffTrackerExt for TurnDiffTracker {
    fn record_exec_command_end(&mut self, command: &[String], exit_code: i32, timed_out: bool) {
        self.record_exec_command_end_at(command, exit_code, timed_out, "", None);
    }
}

#[test]
fn changed_diff_emission_deduplicates_and_preserves_clear_transition() {
    let mut tracker = TurnDiffTracker::new();
    assert_eq!(tracker.take_unified_diff_if_changed(), None);

    tracker.unified_diff = Some("first diff".to_string());
    assert_eq!(
        tracker.take_unified_diff_if_changed(),
        Some("first diff".to_string())
    );
    assert_eq!(tracker.take_unified_diff_if_changed(), None);

    tracker.unified_diff = Some("second diff".to_string());
    assert_eq!(
        tracker.take_unified_diff_if_changed(),
        Some("second diff".to_string())
    );

    tracker.unified_diff = None;
    assert_eq!(tracker.take_unified_diff_if_changed(), Some(String::new()));
    assert_eq!(tracker.take_unified_diff_if_changed(), None);
}

#[test]
fn direct_shell_mutation_paths_are_exact_and_complex_scripts_fall_back() {
    let dir = tempdir().expect("tempdir");
    let exact = command_mutation_paths(&["touch".into(), "src/foo.rs".into()], Some(dir.path()))
        .expect("direct touch path");
    assert_eq!(exact, BTreeSet::from([dir.path().join("src/foo.rs")]));

    assert!(
        command_mutation_paths(
            &[
                "sh".into(),
                "-c".into(),
                "touch src/foo.rs && touch src/bar.rs".into()
            ],
            Some(dir.path()),
        )
        .is_none(),
        "compound scripts must retain conservative unknown invalidation"
    );
}

#[test]
fn validation_commands_do_not_clear_generic_mutation_revision() {
    let mut tracker = TurnDiffTracker::new();
    tracker.record_exec_command_end(
        &[
            "pwsh".to_string(),
            "-Command".to_string(),
            "Set-Content -LiteralPath a.txt -Value changed".to_string(),
        ],
        0,
        false,
    );
    assert_eq!(tracker.current_mutation_revision(), 1);

    tracker.record_exec_command_end(&["cargo".into(), "check".into()], 0, false);
    tracker.record_exec_command_end(
        &["cargo".into(), "test".into(), "selected_case".into()],
        0,
        false,
    );
    assert_eq!(tracker.current_mutation_revision(), 1);

    tracker.record_exec_command_end(
        &[
            "sh".into(),
            "-c".into(),
            "cargo test && touch marker".into(),
        ],
        0,
        false,
    );
    assert_eq!(tracker.current_mutation_revision(), 2);
}

#[test]
fn failed_or_timed_out_mutators_still_create_unknown_mutation_state() {
    for timed_out in [false, true] {
        let mut tracker = TurnDiffTracker::new();
        tracker.record_exec_command_end(
            &[
                "pwsh".to_string(),
                "-Command".to_string(),
                "Set-Content -LiteralPath a.txt -Value changed; exit 1".to_string(),
            ],
            1,
            timed_out,
        );
        assert_eq!(tracker.current_mutation_revision(), 1);
    }
}

#[test]
fn just_fix_is_a_mutation_and_cannot_validate_its_own_edits() {
    let mut tracker = TurnDiffTracker::new();
    tracker.record_unknown_mutation();
    tracker.record_exec_command_end(&["cargo".into(), "check".into()], 0, false);
    assert_eq!(tracker.current_mutation_revision(), 1);

    tracker.record_exec_command_end(
        &[
            "just".into(),
            "fix".into(),
            "-p".into(),
            "codex-core".into(),
        ],
        0,
        false,
    );
    assert_eq!(tracker.current_mutation_revision(), 2);
}

#[test]
fn read_only_shell_commands_do_not_create_mutation_state() {
    let mut tracker = TurnDiffTracker::new();
    tracker.record_exec_command_end(&["git".into(), "status".into()], 0, false);
    tracker.record_exec_command_end(&["git".into(), "log".into(), "-1".into()], 0, false);
    tracker.record_exec_command_end(
        &["cmd".into(), "/c".into(), "type".into(), "README.md".into()],
        0,
        false,
    );
    tracker.record_exec_command_end(
        &[
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "Get-Content README.md".into(),
        ],
        0,
        false,
    );
    tracker.record_exec_command_end(
        &[
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "$ErrorActionPreference = 'Stop'; Write-Output 'owners'; Get-Content README.md -Raw; Select-String -Path README.md -Pattern owner".into(),
        ],
        0,
        false,
    );
    tracker.record_exec_command_end(
        &[
            "pwsh".into(),
            "-Command".into(),
            "Get-ChildItem . | Select-Object -First 1; git status --short".into(),
        ],
        0,
        false,
    );
    tracker.record_exec_command_end(
        &[
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "$path = 'README.md'; $lines = Get-Content -LiteralPath $path; foreach ($range in @(@(0, 5), @(10, 15))) { $start = $range[0]; $end = $range[1]; $lines[$start..$end] }".into(),
        ],
        0,
        false,
    );
    assert_eq!(tracker.current_mutation_revision(), 0);
}

#[test]
fn direct_file_read_shells_still_reject_composed_mutations() {
    for command in [
        vec![
            "cmd".into(),
            "/c".into(),
            "type".into(),
            "README.md".into(),
            ">".into(),
            "copy.txt".into(),
        ],
        vec![
            "cmd".into(),
            "/c".into(),
            "type README.md & del README.md".into(),
        ],
        vec![
            "powershell.exe".into(),
            "-Command".into(),
            "Get-Content README.md; Set-Content README.md changed".into(),
        ],
        vec![
            "powershell.exe".into(),
            "-Command".into(),
            "Get-Content README.md; & ./rewrite.ps1".into(),
        ],
        vec![
            "powershell.exe".into(),
            "-Command".into(),
            "Get-Content README.md; python edit.py".into(),
        ],
        vec![
            "powershell.exe".into(),
            "-Command".into(),
            "$result = (python edit.py)".into(),
        ],
        vec![
            "powershell.exe".into(),
            "-Command".into(),
            "$file.Delete()".into(),
        ],
        vec![
            "powershell.exe".into(),
            "-Command".into(),
            "$result = $file.Delete()".into(),
        ],
        vec![
            "powershell.exe".into(),
            "-Command".into(),
            "$file.IsReadOnly = $false".into(),
        ],
    ] {
        assert!(
            command_may_mutate(&command),
            "composed shell command must fail closed: {command:?}"
        );
    }
}

#[test]
fn arbitrary_script_runners_fail_closed_as_possible_mutations() {
    for command in [
        vec!["python".into(), "edit.py".into()],
        vec!["node".into(), "rewrite.js".into()],
        vec!["custom-codegen.exe".into()],
    ] {
        assert!(
            command_may_mutate(&command),
            "unknown executable must fail closed: {command:?}"
        );
    }

    // Test runners execute user code and may write workspace files.
    assert!(command_may_mutate(&[
        "python".into(),
        "-m".into(),
        "pytest".into(),
    ]));
    assert!(!command_may_mutate(&["cargo".into(), "check".into()]));
}

#[test]
fn mutation_classification_separates_known_mutators_from_uncertain_commands() {
    assert!(matches!(
        command_mutation(
            &[
                "powershell.exe".into(),
                "-Command".into(),
                "Set-Content out.txt changed".into()
            ],
            None,
        ),
        CommandMutation::KnownMutation { .. }
    ));
    assert!(matches!(
        command_mutation(&["python".into(), "edit.py".into()], None),
        CommandMutation::KnownMutation { .. }
    ));
    assert_eq!(
        command_mutation(&["custom-codegen.exe".into()], None),
        CommandMutation::Uncertain
    );
}

#[test]
fn unchanged_uncertain_command_does_not_advance_mutation_revision() {
    let mut tracker = TurnDiffTracker::new();
    tracker.record_exec_command_end_with_mutation_at(
        &["custom-inspector.exe".into()],
        0,
        false,
        "local",
        None,
        resolve_uncertain_command_observation(Some(false)),
    );

    assert_eq!(tracker.current_mutation_revision(), 0);
}

#[test]
fn failed_and_timed_out_known_mutators_still_invalidate_immediately() {
    for (exit_code, timed_out) in [(1, false), (124, true)] {
        let mut tracker = TurnDiffTracker::new();
        let command = [
            "powershell.exe".into(),
            "-Command".into(),
            "Set-Content out.txt changed".into(),
        ];
        let mutation = command_mutation(&command, None);
        tracker.record_exec_command_end_with_mutation_at(
            &command, exit_code, timed_out, "local", None, mutation,
        );
        assert_eq!(tracker.current_mutation_revision(), 1);
    }
}

#[tokio::test]
async fn uncertain_command_observation_detects_tracked_and_untracked_changes() {
    let dir = tempdir().expect("tempdir");
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .expect("run git init")
            .success()
    );
    fs::write(dir.path().join("tracked.txt"), "before\n").expect("seed tracked file");
    assert!(
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(dir.path())
            .status()
            .expect("git add tracked file")
            .success()
    );

    let before_tracked = crate::git_workspace::capture_workspace_evidence_identity(dir.path())
        .await
        .expect("tracked baseline");
    fs::write(dir.path().join("tracked.txt"), "after\n").expect("change tracked file");
    let after_tracked = crate::git_workspace::capture_workspace_evidence_identity(dir.path())
        .await
        .expect("tracked result");
    assert!(matches!(
        resolve_uncertain_command_observation(Some(before_tracked != after_tracked)),
        CommandMutation::KnownMutation { .. }
    ));

    let before_untracked = after_tracked;
    fs::write(dir.path().join("untracked.txt"), "new\n").expect("create untracked file");
    let after_untracked = crate::git_workspace::capture_workspace_evidence_identity(dir.path())
        .await
        .expect("untracked result");
    let observed = resolve_uncertain_command_observation(Some(before_untracked != after_untracked));
    assert!(matches!(observed, CommandMutation::KnownMutation { .. }));

    let mut tracker = TurnDiffTracker::new();
    tracker.record_exec_command_end_with_mutation_at(
        &["custom-codegen.exe".into()],
        0,
        false,
        "local",
        None,
        observed,
    );
    assert_eq!(tracker.current_mutation_revision(), 1);
}

#[test]
fn mutation_boundary_mutating_validation_flags_and_package_scripts_fail_closed() {
    for command in [
        vec!["eslint".into(), "--fix".into(), "src".into()],
        vec!["ruff".into(), "check".into(), "--fix".into()],
        vec!["npm".into(), "run".into(), "build".into()],
        vec!["pnpm".into(), "test:update-snapshots".into()],
        vec!["jest".into(), "-u".into()],
    ] {
        assert!(
            command_may_mutate(&command),
            "validation writer must advance mutation state: {command:?}"
        );
    }
}

#[test]
fn mutation_boundary_git_global_options_preserve_read_write_semantics() {
    for command in [
        vec!["git".into(), "-C".into(), ".".into(), "log".into()],
        vec![
            "git".into(),
            "-c".into(),
            "core.pager=cat".into(),
            "log".into(),
            "-1".into(),
        ],
        vec!["git".into(), "--no-pager".into(), "show".into()],
    ] {
        assert!(
            !command_may_mutate(&command),
            "history read must remain non-mutating: {command:?}"
        );
        assert!(command_reads_repository_history(&command));
    }
    assert!(command_may_mutate(&[
        "git".into(),
        "-C".into(),
        ".".into(),
        "add".into(),
        "src/lib.rs".into(),
    ]));
}

#[test]
fn common_in_place_mutators_are_classified_as_mutations() {
    for command in [
        vec!["chmod".into(), "+x".into(), "run.sh".into()],
        vec!["touch".into(), "a.txt".into()],
        vec!["truncate".into(), "-s".into(), "0".into(), "a.txt".into()],
        vec!["sed".into(), "-i".into(), "s/a/b/".into(), "a.txt".into()],
        vec![
            "sed".into(),
            "--in-place".into(),
            "s/a/b/".into(),
            "a.txt".into(),
        ],
        vec![
            "perl".into(),
            "-pi".into(),
            "-e".into(),
            "s/a/b/".into(),
            "a.txt".into(),
        ],
        vec!["dd".into(), "if=input".into(), "of=output".into()],
        vec!["rsync".into(), "source".into(), "dest".into()],
        vec!["patch".into(), "-p1".into()],
        vec!["patch".into(), "-n".into(), "-p1".into()],
    ] {
        assert!(
            command_may_mutate(&command),
            "expected mutator: {command:?}"
        );
    }
}

#[tokio::test]
async fn accumulates_add_then_update_as_single_add() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());

    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+foo\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);

    let update = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: a.txt\n@@\n foo\n+bar\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &update);

    let right_oid = git_blob_sha1_hex("foo\nbar\n");
    let expected = format!(
        r#"diff --git a/a.txt b/a.txt
new file mode {REGULAR_FILE_MODE}
index {ZERO_OID}..{right_oid}
--- {DEV_NULL}
+++ b/a.txt
@@ -0,0 +1,2 @@
+foo
+bar
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn invalidated_tracker_suppresses_existing_diff() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());

    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+foo\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);

    tracker.invalidate();

    assert_eq!(tracker.get_unified_diff(), None);
}

#[tokio::test]
async fn tracks_same_absolute_path_across_multiple_environments() {
    let dir = tempdir().expect("tempdir");
    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: shared.txt\n+content\n*** End Patch",
    )
    .await;

    let mut tracker = TurnDiffTracker::with_environment_display_roots([
        ("local".to_string(), dir.path().to_path_buf()),
        ("remote".to_string(), dir.path().to_path_buf()),
    ]);
    tracker.track_delta("remote", &add);
    tracker.track_delta("local", &add);

    let right_oid = git_blob_sha1_hex("content\n");
    let expected = format!(
        r#"diff --git a/local/shared.txt b/local/shared.txt
new file mode {REGULAR_FILE_MODE}
index {ZERO_OID}..{right_oid}
--- {DEV_NULL}
+++ b/local/shared.txt
@@ -0,0 +1 @@
+content
diff --git a/remote/shared.txt b/remote/shared.txt
new file mode {REGULAR_FILE_MODE}
index {ZERO_OID}..{right_oid}
--- {DEV_NULL}
+++ b/remote/shared.txt
@@ -0,0 +1 @@
+content
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn accumulates_delete() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("b.txt"), "x\n").expect("seed file");

    let mut tracker = tracker_with_root(dir.path());
    let delete = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Delete File: b.txt\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &delete);

    let left_oid = git_blob_sha1_hex("x\n");
    let expected = format!(
        r#"diff --git a/b.txt b/b.txt
deleted file mode {REGULAR_FILE_MODE}
index {left_oid}..{ZERO_OID}
--- a/b.txt
+++ {DEV_NULL}
@@ -1 +0,0 @@
-x
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn accumulates_move_and_update() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("src.txt"), "line\n").expect("seed file");

    let mut tracker = tracker_with_root(dir.path());
    let update = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: src.txt\n*** Move to: dst.txt\n@@\n-line\n+line2\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &update);

    let left_oid = git_blob_sha1_hex("line\n");
    let right_oid = git_blob_sha1_hex("line2\n");
    let expected = format!(
        r#"diff --git a/src.txt b/dst.txt
index {left_oid}..{right_oid}
--- a/src.txt
+++ b/dst.txt
@@ -1 +1 @@
-line
+line2
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn pure_rename_yields_no_diff() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("old.txt"), "same\n").expect("seed file");

    let mut tracker = tracker_with_root(dir.path());
    let rename = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n same\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &rename);

    assert_eq!(tracker.get_unified_diff(), None);
}

#[tokio::test]
async fn add_over_existing_file_becomes_update() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("dup.txt"), "before\n").expect("seed file");

    let mut tracker = tracker_with_root(dir.path());
    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: dup.txt\n+after\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);

    let left_oid = git_blob_sha1_hex("before\n");
    let right_oid = git_blob_sha1_hex("after\n");
    let expected = format!(
        r#"diff --git a/dup.txt b/dup.txt
index {left_oid}..{right_oid}
--- a/dup.txt
+++ b/dup.txt
@@ -1 +1 @@
-before
+after
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn delete_then_readd_same_path_becomes_update() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("cycle.txt"), "before\n").expect("seed file");

    let mut tracker = tracker_with_root(dir.path());
    let delete = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Delete File: cycle.txt\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &delete);

    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: cycle.txt\n+after\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);

    let left_oid = git_blob_sha1_hex("before\n");
    let right_oid = git_blob_sha1_hex("after\n");
    let expected = format!(
        r#"diff --git a/cycle.txt b/cycle.txt
index {left_oid}..{right_oid}
--- a/cycle.txt
+++ b/cycle.txt
@@ -1 +1 @@
-before
+after
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn move_over_existing_destination_without_content_change_deletes_source_only() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "same\n").expect("seed source");
    fs::write(dir.path().join("b.txt"), "same\n").expect("seed destination");

    let mut tracker = tracker_with_root(dir.path());
    let move_overwrite = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n same\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &move_overwrite);

    let left_oid = git_blob_sha1_hex("same\n");
    let expected = format!(
        r#"diff --git a/a.txt b/a.txt
deleted file mode {REGULAR_FILE_MODE}
index {left_oid}..{ZERO_OID}
--- a/a.txt
+++ {DEV_NULL}
@@ -1 +0,0 @@
-same
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn move_over_existing_destination_with_content_change_deletes_source_and_updates_destination()
{
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "from\n").expect("seed source");
    fs::write(dir.path().join("b.txt"), "existing\n").expect("seed destination");

    let mut tracker = tracker_with_root(dir.path());
    let move_overwrite = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-from\n+new\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &move_overwrite);

    let left_oid_a = git_blob_sha1_hex("from\n");
    let left_oid_b = git_blob_sha1_hex("existing\n");
    let right_oid_b = git_blob_sha1_hex("new\n");
    let expected = format!(
        r#"diff --git a/a.txt b/a.txt
deleted file mode {REGULAR_FILE_MODE}
index {left_oid_a}..{ZERO_OID}
--- a/a.txt
+++ {DEV_NULL}
@@ -1 +0,0 @@
-from
diff --git a/b.txt b/b.txt
index {left_oid_b}..{right_oid_b}
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-existing
+new
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn preserves_committed_change_order_with_delete_then_move_overwrite() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "from\n").expect("seed source");
    fs::write(dir.path().join("b.txt"), "existing\n").expect("seed destination");

    let mut tracker = tracker_with_root(dir.path());
    let delete_destination = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Delete File: b.txt\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &delete_destination);
    let move_source = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-from\n+new\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &move_source);

    let left_oid_a = git_blob_sha1_hex("from\n");
    let left_oid_b = git_blob_sha1_hex("existing\n");
    let right_oid_b = git_blob_sha1_hex("new\n");
    let expected = format!(
        r#"diff --git a/a.txt b/a.txt
deleted file mode {REGULAR_FILE_MODE}
index {left_oid_a}..{ZERO_OID}
--- a/a.txt
+++ {DEV_NULL}
@@ -1 +0,0 @@
-from
diff --git a/b.txt b/b.txt
index {left_oid_b}..{right_oid_b}
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-existing
+new
"#,
    );
    assert_eq!(tracker.get_unified_diff(), Some(expected));
}

#[tokio::test]
async fn reuses_rendered_diffs_for_unchanged_paths() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());

    let add_a = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+one\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add_a);
    assert_eq!(tracker.rendered_diff_count(), 1);

    let add_b = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: b.txt\n+two\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add_b);

    assert_eq!(tracker.rendered_diff_count(), 2);
    assert_eq!(
        tracker.get_unified_diff(),
        tracker.get_unified_diff(),
        "reading the cached aggregate must not render file diffs",
    );
    assert_eq!(tracker.rendered_diff_count(), 2);
}

#[tokio::test]
async fn repeated_updates_only_rerender_the_touched_path() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());

    for patch in [
        "*** Begin Patch\n*** Add File: stable.txt\n+stable\n*** End Patch".to_string(),
        "*** Begin Patch\n*** Add File: hot.txt\n+value 0\n*** End Patch".to_string(),
    ] {
        tracker.track_delta("", &apply_verified_patch(dir.path(), &patch).await);
    }

    for value in 1..=40 {
        let patch = format!(
            "*** Begin Patch\n*** Update File: hot.txt\n@@\n-value {}\n+value {value}\n*** End Patch",
            value - 1,
        );
        tracker.track_delta("", &apply_verified_patch(dir.path(), &patch).await);
    }

    assert_eq!(tracker.rendered_diff_count(), 42);
}

#[tokio::test]
async fn bulk_patch_deletion_batches_index_modes_and_preserves_executable_mode() {
    let dir = tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "--quiet"]);
    let mut patch = "*** Begin Patch\n".to_owned();
    for index in 0..130 {
        fs::write(dir.path().join(format!("file-{index}.txt")), "original\n").unwrap();
        patch.push_str(&format!("*** Delete File: file-{index}.txt\n"));
    }
    git(&["add", "."]);
    git(&["update-index", "--chmod=+x", "file-0.txt"]);
    patch.push_str("*** End Patch\n");
    let mut tracker = tracker_with_root(dir.path());
    let delta = apply_verified_patch(dir.path(), &patch).await;
    let missing = tracker.missing_mode_paths("", &delta);
    PATCH_MODE_QUERY_COUNT.with(|count| count.set(0));
    let modes = resolve_patch_index_modes(dir.path(), missing).await;
    assert_eq!(PATCH_MODE_QUERY_COUNT.with(std::cell::Cell::get), 3);
    tracker.set_patch_modes("", modes);
    tracker.track_delta("", &delta);
    let diff = tracker.take_unified_diff_if_changed().unwrap();
    assert_eq!(diff.matches("diff --git ").count(), 130);
    assert_eq!(diff.matches("deleted file mode 100755").count(), 1);
    assert_eq!(diff.matches("deleted file mode 100644").count(), 129);
    assert!(!dir.path().join("file-0.txt").exists());
}

#[tokio::test]
async fn repeated_patch_updates_reuse_the_baseline_blob_identity() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("hot.txt"), "baseline\n").unwrap();
    let mut tracker = tracker_with_root(dir.path());
    let mut prior = "baseline".to_owned();
    GIT_BLOB_HASH_COUNT.with(|count| count.set(0));
    for index in 0..8 {
        let next = format!("revision {index}");
        let delta = apply_verified_patch(
            dir.path(),
            &format!(
                "*** Begin Patch\n*** Update File: hot.txt\n@@\n-{prior}\n+{next}\n*** End Patch"
            ),
        )
        .await;
        tracker.track_delta("", &delta);
        assert_eq!(
            GIT_BLOB_HASH_COUNT.with(std::cell::Cell::get),
            index + 2,
            "hash the baseline once and each new content revision once"
        );
        let diff = tracker.take_unified_diff_if_changed().unwrap();
        assert!(diff.contains(&format!(
            "index {}..{}",
            git_blob_sha1_hex("baseline\n"),
            git_blob_sha1_hex(&format!("{next}\n"))
        )));
        prior = next;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn patches_keep_case_distinct_paths_independent_across_renames() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("Foo.rs"), "upper\n").unwrap();
    fs::write(dir.path().join("foo.rs"), "lower\n").unwrap();
    let mut tracker = tracker_with_root(dir.path());
    let edit = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Update File: Foo.rs\n@@\n-upper\n+UPPER\n*** Update File: foo.rs\n@@\n-lower\n+LOWER\n*** End Patch").await;
    tracker.track_delta("", &edit);
    let diff = tracker.take_unified_diff_if_changed().unwrap();
    assert!(diff.contains("--- a/Foo.rs\n+++ b/Foo.rs"));
    assert!(diff.contains("--- a/foo.rs\n+++ b/foo.rs"));
    let rename = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Update File: Foo.rs\n*** Move to: Renamed.rs\n@@\n-UPPER\n+renamed\n*** End Patch").await;
    tracker.track_delta("", &rename);
    let diff = tracker.take_unified_diff_if_changed().unwrap();
    assert!(diff.contains("--- a/Foo.rs\n+++ b/Renamed.rs"));
    assert!(diff.contains("--- a/foo.rs\n+++ b/foo.rs"));
    assert_eq!(
        fs::read_to_string(dir.path().join("foo.rs")).unwrap(),
        "LOWER\n"
    );
}

#[cfg(unix)]
#[test]
fn tracking_keys_preserve_non_utf8_native_paths() {
    use std::os::unix::ffi::OsStrExt;
    let first = Path::new(std::ffi::OsStr::from_bytes(b"file-\xff"));
    let second = Path::new(std::ffi::OsStr::from_bytes(b"file-\xfe"));
    let first_key = TrackedPath::new("local", first);
    let second_key = TrackedPath::new("local", second);
    assert_ne!(first_key, second_key);
    assert_eq!(first_key.path.as_os_str().as_bytes(), b"file-\xff");
    let mut contents = HashMap::new();
    contents.insert(first_key, "first");
    contents.insert(second_key, "second");
    assert_eq!(contents.len(), 2);
}

#[cfg(windows)]
#[test]
fn windows_tracking_aliases_preserve_display_spelling() {
    let plain = TrackedPath::new("local", Path::new(r"C:\Workspace\src\Mixed.rs"));
    let alias = TrackedPath::new("local", Path::new(r"\\?\c:\workspace\src\mixed.rs"));
    assert_eq!(plain, alias);
    assert_eq!(plain.path, PathBuf::from(r"C:\Workspace\src\Mixed.rs"));
    let tracker = TurnDiffTracker::with_environment_display_roots([(
        "local".to_owned(),
        PathBuf::from(r"c:\workspace"),
    )]);
    assert_eq!(tracker.display_path(&plain), "src/Mixed.rs");
    assert_ne!(plain, TrackedPath::new("other", &alias.path));
    assert_eq!(
        TrackedPath::new("local", Path::new(r"C:\Workspace\Édit.rs")),
        TrackedPath::new("local", Path::new(r"c:\workspace\édit.rs"))
    );
}

#[test]
fn large_rewrite_returns_promptly_and_preserves_exact_content() {
    let dir = tempdir().expect("tempdir");
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .expect("run git init")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "core.autocrlf", "false"])
            .current_dir(dir.path())
            .status()
            .expect("disable line ending conversion")
            .success()
    );
    let old_content = (0..48_000)
        .map(|line| format!("old line {line:05}\n"))
        .collect::<String>();
    let new_content = (0..48_000)
        .map(|line| format!("new line {line:05}\n"))
        .collect::<String>();
    let path = dir.path().join("large.txt");
    fs::write(&path, &old_content).expect("seed large file");
    assert!(
        Command::new("git")
            .args(["add", "large.txt"])
            .current_dir(dir.path())
            .status()
            .expect("run git add")
            .success()
    );
    let mut tracker = tracker_with_root(dir.path());
    let tracked_path = TrackedPath::new("", &path);
    let old_tracked = tracker.tracked_content(&tracked_path, &old_content);
    let new_tracked = tracker.tracked_content(&tracked_path, &new_content);

    let started = Instant::now();
    let diff = tracker
        .render_diff(
            &tracked_path,
            Some(&old_tracked),
            &tracked_path,
            Some(&new_tracked),
        )
        .expect("complete rewrite should produce a diff");

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "large rewrite took {:?}",
        started.elapsed(),
    );
    let result = apply_git_patch(&ApplyGitRequest {
        cwd: dir.path().to_path_buf(),
        diff,
        revert: false,
        preflight: false,
    })
    .expect("apply generated diff");
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(
        fs::read_to_string(path).expect("read large file"),
        new_content
    );
}
