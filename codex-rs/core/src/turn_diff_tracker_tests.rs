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
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tempfile::tempdir;
use tokio::sync::Mutex;

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

fn verify_local_proof_command() -> Vec<String> {
    vec![
        "just".into(),
        "verify-local".into(),
        "--fast".into(),
        "--json".into(),
    ]
}

async fn wait_for_render_pause(pause: &TestRenderPause) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !pause.started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn diff render should start");
}

trait TestTurnDiffTrackerExt {
    fn record_exec_command_end(&mut self, command: &[String], exit_code: i32, timed_out: bool);
}

impl TestTurnDiffTrackerExt for TurnDiffTracker {
    fn record_exec_command_end(&mut self, command: &[String], exit_code: i32, timed_out: bool) {
        self.record_exec_command_end_at(command, exit_code, timed_out, "", None);
    }
}

#[tokio::test]
async fn validation_freshness_tracks_format_timeout_broad_success_and_staleness() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+foo\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);

    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::None
    );

    tracker.record_exec_command_end(&["just".into(), "fmt".into()], 0, false);
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::FormatOnly
    );

    tracker.record_exec_command_end(&["cargo".into(), "test".into()], 1, true);
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::TimedOut
    );

    tracker.record_exec_command_end(
        &[
            "just".into(),
            "test-fast".into(),
            "-E".into(),
            "test(core) | test(protocol)".into(),
        ],
        0,
        false,
    );
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::AdvisoryBroadFilter
    );
    assert!(tracker.has_unvalidated_mutation());

    tracker.record_exec_command_end(&["cargo".into(), "check".into()], 0, false);
    assert!(!tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::PassedAfterLastMutation
    );

    let second = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: b.txt\n+bar\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &second);
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::StaleAfterLastMutation
    );
}

#[tokio::test]
async fn verified_validation_clears_only_the_paths_it_covered() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    for (path, content) in [("a.txt", "foo"), ("b.txt", "bar")] {
        let delta = apply_verified_patch(
            dir.path(),
            &format!("*** Begin Patch\n*** Add File: {path}\n+{content}\n*** End Patch"),
        )
        .await;
        tracker.track_delta("", &delta);
    }

    assert!(!tracker.record_verified_validation(
        verify_local_proof_command(),
        "",
        &[PathBuf::from("a.txt")],
        false,
    ));
    assert!(tracker.has_unvalidated_mutation());

    assert!(tracker.record_verified_validation(
        verify_local_proof_command(),
        "",
        &[PathBuf::from("b.txt")],
        false,
    ));
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::PassedAfterLastMutation
    );

    tracker.record_unknown_mutation();
    assert!(tracker.record_verified_validation(verify_local_proof_command(), "", &[], true,));
}

#[tokio::test]
async fn scoped_shell_validation_clears_only_matching_changed_paths() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    for path in ["tests/a.py", "src/b.rs"] {
        let delta = apply_verified_patch(
            dir.path(),
            &format!("*** Begin Patch\n*** Add File: {path}\n+changed\n*** End Patch"),
        )
        .await;
        tracker.track_delta("", &delta);
    }

    tracker.record_exec_command_end_at(
        &["pytest".into(), "tests/a.py".into()],
        0,
        false,
        "",
        Some(dir.path()),
    );

    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::ScopedValidationIncomplete
    );
    assert!(tracker.record_verified_validation(
        verify_local_proof_command(),
        "",
        &[PathBuf::from("src/b.rs")],
        false,
    ));
}

#[tokio::test]
async fn package_scoped_validation_maps_codex_package_to_its_crate_directory() {
    let dir = tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("codex-rs/core/src")).expect("core directory");
    fs::create_dir_all(dir.path().join("codex-rs/protocol/src")).expect("protocol directory");
    fs::write(
        dir.path().join("codex-rs/Cargo.toml"),
        "[workspace]\nmembers = [\"core\", \"protocol\"]\n",
    )
    .expect("workspace manifest");
    fs::write(
        dir.path().join("codex-rs/core/Cargo.toml"),
        "[package]\nname = \"codex-core\"\nversion = \"0.0.0\"\n",
    )
    .expect("core manifest");
    fs::write(
        dir.path().join("codex-rs/protocol/Cargo.toml"),
        "[package]\nname = \"codex-protocol\"\nversion = \"0.0.0\"\n",
    )
    .expect("protocol manifest");
    let mut tracker = tracker_with_root(dir.path());
    for path in ["codex-rs/core/src/a.rs", "codex-rs/protocol/src/b.rs"] {
        let delta = apply_verified_patch(
            dir.path(),
            &format!("*** Begin Patch\n*** Add File: {path}\n+changed\n*** End Patch"),
        )
        .await;
        tracker.track_delta("", &delta);
    }

    tracker.record_exec_command_end_at(
        &[
            "cargo".into(),
            "check".into(),
            "-p".into(),
            "codex-core".into(),
        ],
        0,
        false,
        "",
        Some(dir.path()),
    );

    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::ScopedValidationIncomplete
    );
    assert!(tracker.record_verified_validation(
        verify_local_proof_command(),
        "",
        &[PathBuf::from("codex-rs/protocol/src/b.rs")],
        false,
    ));
}

#[tokio::test]
async fn unproven_validator_scope_does_not_clear_unrelated_changes() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    let delta = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: src/a.ts\n+changed\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &delta);

    tracker.record_exec_command_end_at(
        &["npm".into(), "test".into()],
        0,
        false,
        "",
        Some(dir.path()),
    );

    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::ScopedValidationIncomplete
    );
}

#[tokio::test]
async fn cargo_audit_does_not_count_as_source_correctness_validation() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    let delta = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: src/lib.rs\n+changed\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &delta);

    tracker.record_exec_command_end_at(
        &["cargo".into(), "audit".into()],
        0,
        false,
        "",
        Some(dir.path()),
    );

    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::ScopedValidationIncomplete
    );
}

#[test]
fn generated_powershell_validation_is_recognized_without_echo_false_positives() {
    let generated = [r#"$out = & just test-fast -p codex-core 2>&1
$code = $LASTEXITCODE
$out | Select-Object -Last 160
exit $code"#
        .to_string()];
    assert!(is_validation_command(&generated));
    assert!(!is_validation_command(&[
        "echo".to_string(),
        "cargo test".to_string()
    ]));
    assert!(
        ValidationFreshnessStatus::None
            .final_warning_message()
            .is_some()
    );
    assert!(
        ValidationFreshnessStatus::PassedAfterLastMutation
            .final_warning_message()
            .is_none()
    );
}

#[test]
fn successful_mutating_shell_commands_create_unknown_unvalidated_state() {
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
    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::None
    );

    tracker.record_exec_command_end(&["cargo".into(), "check".into()], 0, false);
    assert!(!tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::PassedAfterLastMutation
    );
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
        assert!(tracker.has_unvalidated_mutation());
    }
}

#[test]
fn observed_noop_overrides_syntactic_mutator_classification() {
    let command = [
        "pwsh".to_string(),
        "-Command".to_string(),
        "Set-Content -LiteralPath a.txt -Value unchanged; exit 1".to_string(),
    ];
    let mut tracker = TurnDiffTracker::new();

    tracker.record_exec_command_end_at_with_observation(
        &command,
        1,
        false,
        "",
        None,
        MutationObservation::Unchanged,
    );
    assert!(!tracker.has_unvalidated_mutation());

    tracker.record_exec_command_end_at_with_observation(
        &command,
        1,
        false,
        "",
        None,
        MutationObservation::Changed,
    );
    assert!(tracker.has_unvalidated_mutation());
}

#[test]
fn just_fix_is_a_mutation_and_cannot_validate_its_own_edits() {
    let mut tracker = TurnDiffTracker::new();
    tracker.record_unknown_mutation();
    tracker.record_exec_command_end(&["cargo".into(), "check".into()], 0, false);
    assert!(!tracker.has_unvalidated_mutation());

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
    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::StaleAfterLastMutation
    );
}

#[test]
fn read_only_shell_commands_do_not_create_mutation_state() {
    let mut tracker = TurnDiffTracker::new();
    tracker.record_exec_command_end(&["git".into(), "status".into()], 0, false);
    assert!(!tracker.has_unvalidated_mutation());
}

#[tokio::test]
async fn command_validation_clears_only_its_environment() {
    let dir = tempdir().expect("tempdir");
    let first_root = dir.path().join("first");
    let second_root = dir.path().join("second");
    fs::create_dir_all(&first_root).expect("first root");
    fs::create_dir_all(&second_root).expect("second root");
    let mut tracker = TurnDiffTracker::with_environment_display_roots([
        ("first".to_string(), first_root.clone()),
        ("second".to_string(), second_root.clone()),
    ]);
    for (environment_id, root) in [("first", &first_root), ("second", &second_root)] {
        let delta = apply_verified_patch(
            root,
            "*** Begin Patch\n*** Add File: src/lib.rs\n+changed\n*** End Patch",
        )
        .await;
        tracker.track_delta(environment_id, &delta);
    }

    tracker.record_exec_command_end_at(
        &["cargo".into(), "check".into()],
        0,
        false,
        "first",
        Some(&first_root),
    );

    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::ScopedValidationIncomplete
    );
    assert!(tracker.record_verified_validation(
        verify_local_proof_command(),
        "second",
        &[PathBuf::from("src/lib.rs")],
        false,
    ));
}

#[tokio::test]
async fn command_validation_uses_exact_environment_when_roots_are_identical() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = TurnDiffTracker::with_environment_display_roots([
        ("first".to_string(), dir.path().to_path_buf()),
        ("second".to_string(), dir.path().to_path_buf()),
    ]);
    let delta = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: src/lib.rs\n+changed\n*** End Patch",
    )
    .await;
    tracker.track_delta("second", &delta);

    tracker.record_exec_command_end_at(
        &["cargo".into(), "check".into()],
        0,
        false,
        "first",
        Some(dir.path()),
    );

    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::ScopedValidationIncomplete
    );
    assert!(tracker.record_verified_validation(
        verify_local_proof_command(),
        "second",
        &[PathBuf::from("src/lib.rs")],
        false,
    ));
}

#[tokio::test]
async fn verified_validation_requires_the_real_command_and_normalizes_paths() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    let delta = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: src/lib.rs\n+changed\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &delta);

    assert!(!tracker.record_verified_validation(
        vec!["echo".into(), "VERIFIED".into()],
        "",
        &[PathBuf::from("src/lib.rs")],
        false,
    ));
    assert!(tracker.has_unvalidated_mutation());
    assert!(tracker.record_verified_validation(
        verify_local_proof_command(),
        "",
        &[dir.path().join("src/../src/lib.rs")],
        false,
    ));
}

#[test]
fn attached_cargo_flags_and_scoped_runner_options_are_not_broad_validation() {
    let dir = tempdir().expect("tempdir");
    let workspace = dir.path().join("codex-rs");
    fs::create_dir_all(workspace.join("utils/nested/src")).expect("nested crate");
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"utils/nested\"]\n",
    )
    .expect("workspace manifest");
    fs::write(
        workspace.join("utils/nested/Cargo.toml"),
        "[package]\nname = \"codex-utils-nested\"\nversion = \"0.0.0\"\n",
    )
    .expect("nested manifest");

    assert_eq!(
        cargo_validation_coverage(
            &[
                "cargo".into(),
                "check".into(),
                "--package=codex-utils-nested".into(),
            ],
            Some(&workspace),
        ),
        ValidationCoverage::Paths(vec![normalize_tracked_path(
            &workspace.join("utils/nested")
        )])
    );
    assert_eq!(
        cargo_validation_coverage(
            &[
                "cargo".into(),
                "check".into(),
                "-pcodex-utils-nested".into(),
            ],
            Some(&workspace),
        ),
        ValidationCoverage::Paths(vec![normalize_tracked_path(
            &workspace.join("utils/nested")
        )])
    );
    assert_eq!(
        cargo_validation_coverage(
            &["cargo".into(), "test".into(), "--test=focused".into()],
            Some(&workspace),
        ),
        ValidationCoverage::ScopedUnknown
    );
    assert_eq!(
        just_validation_coverage(
            &["just".into(), "test-fast".into(), "focused_filter".into()],
            Some(&workspace),
        ),
        ValidationCoverage::ScopedUnknown
    );
    assert_eq!(
        pytest_validation_coverage(
            &["pytest".into(), "--ignore".into(), "tests/slow".into()],
            Some(&workspace),
        ),
        ValidationCoverage::ScopedUnknown
    );
    for command in [
        vec![
            "cargo".into(),
            "nextest".into(),
            "run".into(),
            "--filter-expr=test(focused)".into(),
        ],
        vec![
            "cargo".into(),
            "nextest".into(),
            "run".into(),
            "-Etest(focused)".into(),
        ],
        vec![
            "cargo".into(),
            "test".into(),
            "--workspace".into(),
            "--exclude=codex-utils-nested".into(),
        ],
        vec!["cargo".into(), "test".into(), "--doc".into()],
        vec!["cargo".into(), "test".into(), "--bench=focused".into()],
        vec!["cargo".into(), "test".into(), "--no-run".into()],
    ] {
        assert_eq!(
            cargo_validation_coverage(&command, Some(&workspace)),
            ValidationCoverage::ScopedUnknown,
            "expected scoped Cargo validation: {command:?}"
        );
    }
    assert!(is_broad_validation_filter_command(&[
        "cargo".into(),
        "nextest".into(),
        "run".into(),
        "--filter-expr=test(one)|test(two)".into(),
    ]));
}

#[test]
fn common_in_place_mutators_invalidate_validation_freshness() {
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

#[test]
fn metadata_mutation_makes_successful_validation_stale() {
    let mut tracker = TurnDiffTracker::new();
    tracker.record_unknown_mutation();
    tracker.record_exec_command_end_at(&["cargo".into(), "check".into()], 0, false, "local", None);
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::PassedAfterLastMutation
    );

    tracker.record_exec_command_end_at(
        &["chmod".into(), "+x".into(), "run.sh".into()],
        0,
        false,
        "local",
        None,
    );

    assert!(tracker.has_unvalidated_mutation());
    assert_eq!(
        tracker.validation_freshness_status(),
        ValidationFreshnessStatus::StaleAfterLastMutation
    );
}

#[cfg(unix)]
#[tokio::test]
async fn executable_add_uses_the_filesystem_mode_in_generated_diff() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().expect("tempdir");
    let delta = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: run.sh\n+#!/bin/sh\n+exit 0\n*** End Patch",
    )
    .await;
    fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755))
        .expect("executable mode");
    let mut tracker = tracker_with_root(dir.path());
    tracker.track_delta("", &delta);

    assert!(
        tracker
            .get_unified_diff()
            .expect("diff")
            .contains("new file mode 100755")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn later_delete_preserves_mode_of_previously_tracked_untracked_executable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("run.sh");
    fs::write(&path, "before\n").expect("seed executable");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable mode");
    let mut tracker = tracker_with_root(dir.path());

    let update = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: run.sh\n@@\n-before\n+after\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &update);
    let delete = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Delete File: run.sh\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &delete);

    assert!(
        tracker
            .get_unified_diff()
            .expect("diff")
            .contains("deleted file mode 100755")
    );
}

#[cfg(unix)]
#[test]
fn tracked_symlink_mode_survives_later_filesystem_deletion() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("tempdir");
    let link = dir.path().join("link.txt");
    symlink("target.txt", &link).expect("create symlink");
    let mut tracker = tracker_with_root(dir.path());
    let tracked_path = TrackedPath::new("", &link);
    let baseline = tracker.tracked_content(&tracked_path, "target.txt");
    assert_eq!(baseline.mode.as_deref(), Some("120000"));
    tracker
        .baseline_by_path
        .insert(tracked_path.clone(), baseline);
    fs::remove_file(&link).expect("remove symlink");

    tracker.refresh_unified_diff();

    assert!(
        tracker
            .get_unified_diff()
            .expect("diff")
            .contains("deleted file mode 120000")
    );
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
    let ordered_patch = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Delete File: b.txt\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-from\n+new\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &ordered_patch);

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
async fn async_render_releases_tracker_mutex_and_discards_stale_result() {
    let dir = tempdir().expect("tempdir");
    let delta = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+one\n*** End Patch",
    )
    .await;
    let pause = Arc::new(TestRenderPause::default());
    let tracker = Arc::new(Mutex::new(tracker_with_root(dir.path())));
    tracker.lock().await.render_pause = Some(Arc::clone(&pause));

    let update = tokio::spawn(TurnDiffTracker::track_delta_async(
        Arc::clone(&tracker),
        String::new(),
        delta,
    ));
    wait_for_render_pause(&pause).await;

    let mut guard = tokio::time::timeout(Duration::from_millis(100), tracker.lock())
        .await
        .expect("diff rendering must not hold the tracker mutex");
    guard.record_unknown_mutation();
    drop(guard);
    pause.released.store(true, Ordering::Release);

    assert!(update.await.expect("update task").is_none());
    assert_eq!(tracker.lock().await.get_unified_diff(), None);
}

#[tokio::test]
async fn cancelled_waiter_does_not_cancel_final_diff_publish() {
    let dir = tempdir().expect("tempdir");
    let delta = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+one\n*** End Patch",
    )
    .await;
    let pause = Arc::new(TestRenderPause::default());
    let tracker = Arc::new(Mutex::new(tracker_with_root(dir.path())));
    tracker.lock().await.render_pause = Some(Arc::clone(&pause));

    let update = tokio::spawn(TurnDiffTracker::track_delta_async(
        Arc::clone(&tracker),
        String::new(),
        delta,
    ));
    wait_for_render_pause(&pause).await;
    update.abort();
    assert!(update.await.expect_err("waiter should be cancelled").is_cancelled());
    pause.released.store(true, Ordering::Release);

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tracker.lock().await.get_unified_diff().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached render worker should publish the final diff");
    assert!(
        tracker
            .lock()
            .await
            .get_unified_diff()
            .expect("final diff")
            .contains("+one")
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
