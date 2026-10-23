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

#[cfg(unix)]
#[tokio::test]
async fn audit151_alias_patch_continuity_rejects_an_intervening_peer_edit() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("real")).unwrap();
    fs::write(dir.path().join("real/shared.txt"), "base\n").unwrap();
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
    let mut tracker = tracker_with_root(dir.path());
    let first = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Update File: alias/shared.txt\n@@\n-base\n+mine\n*** End Patch").await;
    tracker.track_delta("local", &first);
    fs::write(dir.path().join("real/shared.txt"), "peer\n").unwrap();
    let second = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Update File: real/shared.txt\n@@\n-peer\n+next\n*** End Patch").await;
    tracker.track_delta("local", &second);
    assert!(tracker.exact_unified_diff().is_err());
    assert!(!tracker.unattributed_display_paths().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn audit151_equal_text_metadata_changes_are_uncertainty_not_progress() {
    use std::os::unix::fs::PermissionsExt;
    for symlink in [false, true] {
        let dir = tempdir().unwrap();
        let path = dir.path().join("own.txt");
        let mut tracker = tracker_with_root(dir.path());
        let owned = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Add File: own.txt\n+mine\n*** End Patch").await;
        tracker.track_delta("local", &owned);
        let revision = tracker.attributed_mutation_revision();
        if symlink {
            fs::write(dir.path().join("target.txt"), "mine\n").unwrap();
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(dir.path().join("target.txt"), &path).unwrap();
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mutation = tracker.attribute_observed_changes("local", Some(dir.path()), None, &BTreeSet::from([path]), Some(LOCAL_FS.as_ref())).await;
        tracker.record_exec_command_end_with_mutation_at(&["inspection".into()], 0, false, "local", Some(dir.path()), mutation);
        assert_eq!(tracker.attributed_mutation_revision(), revision);
        assert_eq!(tracker.unattributed_display_paths(), ["own.txt"]);
    }
}

#[tokio::test]
async fn audit147_operation_history_is_bounded_and_never_repairs_net_attribution() {
    let root = tempdir().unwrap();
    let mut tracker = tracker_with_root(root.path());
    for index in 0..70 {
        let delta = apply_verified_patch(
            root.path(),
            &format!("*** Begin Patch\n*** Add File: file-{index}\n+owned\n*** End Patch"),
        )
        .await;
        tracker.track_delta("", &delta);
    }
    let summary = tracker.owned_operation_summary();
    assert_eq!(
        summary["available_operations"].as_array().unwrap().len(),
        64
    );
    assert_eq!(summary["exact_operation_history_complete"], false);
    assert!(summary["retained_bytes"].as_u64().unwrap() <= 8 * 1024 * 1024);
    assert!(tracker.owned_operation_receipt(1).is_none());
    assert!(tracker.owned_operation_receipt(70).is_some());
    tracker.record_unknown_mutation();
    assert!(tracker.exact_unified_diff().is_err());
    assert_eq!(
        tracker.owned_operation_receipt(70).unwrap()["current_source_verified"],
        false
    );
}

#[tokio::test]
async fn audit147_new_discontinuities_have_call_paths_and_preimage_hashes() {
    let dir = tempdir().unwrap();
    let mut tracker = tracker_with_root(dir.path());
    let seed = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a\n+owned\n*** Add File: b\n+owned\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &seed);
    for (path, call) in [("a", "first-patch"), ("b", "second-patch")] {
        fs::write(dir.path().join(path), "foreign\n").unwrap();
        let delta = apply_verified_patch(
            dir.path(),
            &format!(
                "*** Begin Patch\n*** Update File: {path}\n@@\n-foreign\n+next owned\n*** End Patch"
            ),
        )
        .await;
        tracker.track_delta_for_call("", &delta, Some(call));
        let warning = tracker
            .take_invalidation_warning()
            .expect("a newly affected scope must be reported");
        assert!(warning.contains(call) && warning.contains(path));
        assert!(warning.contains("expected_sha256") && warning.contains("observed_sha256"));
        assert_eq!(tracker.take_invalidation_warning(), None);
    }
}

#[test]
fn audit147_unknown_empty_delta_advances_evidence_without_owned_progress() {
    let mut tracker = TurnDiffTracker::new();
    tracker.track_delta("local", &AppliedPatchDelta::default());
    assert_eq!(tracker.current_mutation_revision(), 0);
    tracker.track_delta("local", &AppliedPatchDelta::unknown());
    assert_eq!(tracker.current_mutation_revision(), 1);
    assert_eq!(tracker.current_validation_revision(), 1);
    assert_eq!(tracker.attributed_mutation_revision(), 0);
    assert!(tracker.has_untracked_changes());
    tracker.track_delta("local", &AppliedPatchDelta::unknown());
    assert_eq!(tracker.current_mutation_revision(), 2);
}

#[tokio::test]
async fn audit147_foreign_tracked_change_is_not_command_progress() {
    let dir = tempdir().unwrap();
    let mut tracker = tracker_with_root(dir.path());
    let delta = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Add File: own.txt\n+mine\n*** Add File: untouched.txt\n+owned\n*** End Patch").await;
    tracker.track_delta("", &delta);
    let owned_revision = tracker.attributed_mutation_revision();
    fs::write(dir.path().join("own.txt"), "foreign\n").unwrap();
    let observed = BTreeSet::from([dir.path().join("own.txt")]);
    let mutation = tracker
        .attribute_observed_changes(
            "",
            Some(dir.path()),
            None,
            &observed,
            Some(LOCAL_FS.as_ref()),
        )
        .await;
    tracker.record_exec_command_end_with_mutation_at(
        &["inspection".into()],
        0,
        false,
        "",
        Some(dir.path()),
        mutation,
    );
    assert_eq!(tracker.attributed_mutation_revision(), owned_revision);
    assert_eq!(tracker.unattributed_display_paths(), ["own.txt"]);
    assert!(tracker.get_unified_diff().unwrap().contains("+owned"));
    assert!(tracker.exact_unified_diff().is_err());
}

fn git_blob_sha1_hex(data: &str) -> String {
    // Independently computed with Python hashlib.sha1 over Git's blob framing:
    // b"blob " + str(len(data.encode())).encode() + b"\0" + data.encode().
    // Do not use the production hashing helper to derive its own expected output.
    match data {
        "foo\nbar\n" => "3bd1f0e29744a1f32b08d5650e62e2e62afb177c",
        "content\n" => "d95f3ad14dee633a758d2e331151e950dd13e4ed",
        "x\n" => "587be6b4c3f93f93c489c0111bba5596147a26cb",
        "line\n" => "a999a0c211215fd28e77d6a7c66ade6ec76ccbcb",
        "line2\n" => "8a6a2d098ecaf90105f1cf2fa90fc4608bb08067",
        "before\n" => "90be1f3056c4f471f977a28497b8d4b392c55a02",
        "after\n" => "294186e497a23bf3fbfde12aacc7f720f668fe9a",
        "same\n" => "1275430f1765c63e539cb0452565563bd6aef6a6",
        "from\n" => "3940df7cd8791c48c101a7543b3a9fd7ef930e78",
        "existing\n" => "cbaf024e5e7fa87bdd8ad76b1393d9973585bc97",
        "new\n" => "3e757656cf36eca53338e520d134963a44f793f8",
        "baseline\n" => "180b47c18ba7e484fc0d7350bc6cad8ec3423ea0",
        "revision 0\n" => "1218ba76483e58f3de96b905927706f27cdbc797",
        "revision 1\n" => "1be5cd3b21f3d4292cce238b68aeb236af358b12",
        "revision 2\n" => "b96803c38675622e42dd918aadfd8ed34b594db6",
        "revision 3\n" => "d99e75ab7d419e0c6170c631b22109df4247793b",
        "revision 4\n" => "61ff198b8142d5b2cba423d15c22be6142bd0ff1",
        "revision 5\n" => "bd3ae8a69d51173515ab75f02334cbad02fc9009",
        "revision 6\n" => "5133ebdeba84f67c951fdc4093d7a56b9d4b8883",
        "revision 7\n" => "b99a891640abeab3c8f45b00116884f117ed2656",
        _ => panic!("missing independently computed blob fixture: {data:?}"),
    }
    .to_string()
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

#[tokio::test]
async fn audit107_patch_chain_excludes_foreign_same_file_edits() {
    for foreign in [false, true] {
        let dir = tempdir().unwrap();
        let file = dir.path().join("owned.txt");
        fs::write(&file, "own = 1\nother = 7\n").unwrap();
        let mut tracker = tracker_with_root(dir.path());
        let first = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Update File: owned.txt\n@@\n-own = 1\n+own = 2\n*** Add File: unaffected.txt\n+owned\n*** End Patch").await;
        tracker.track_delta("", &first);
        if foreign {
            fs::write(&file, "own = 2\nother = 9\n").unwrap();
        }
        let next = apply_verified_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: owned.txt\n@@\n-own = 2\n+own = 3\n*** End Patch",
        )
        .await;
        tracker.track_delta("", &next);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            if foreign {
                "own = 3\nother = 9\n"
            } else {
                "own = 3\nother = 7\n"
            }
        );
        if foreign {
            assert!(tracker.exact_unified_diff().is_err());
            let partial = tracker.get_unified_diff().unwrap();
            assert!(
                partial.contains("+owned"),
                "unaffected exact edits must survive"
            );
            assert!(!partial.contains("+other = 9"));
        } else {
            let exact = tracker.exact_unified_diff().unwrap().unwrap();
            assert!(exact.contains("-own = 1"));
            assert!(exact.contains("+own = 3"));
            assert!(!tracker.has_untracked_changes());
        }
    }
}

#[tokio::test]
async fn uncertain_patch_contents_never_reach_existing_test_advisories() {
    let dir = tempdir().unwrap();
    let original = "import unittest\nEXPECTED = 1\nclass TestContract(unittest.TestCase):\n    def test_contract(self):\n        self.assertEqual(2, EXPECTED)\n";
    fs::write(dir.path().join("test_contract.py"), original).unwrap();
    fs::write(
        dir.path().join("test_sibling.py"),
        "def test_sibling():\n    assert True\n",
    )
    .unwrap();
    let mut tracker = tracker_with_root(dir.path());
    let first = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Update File: test_contract.py\n@@\n-EXPECTED = 1\n+EXPECTED = 2\n*** Update File: test_sibling.py\n@@\n-    assert True\n+    assert 1 == 1\n*** End Patch").await;
    tracker.track_delta("", &first);
    fs::write(
        dir.path().join("test_contract.py"),
        original.replace("EXPECTED = 1", "EXPECTED = 2").replace(
            "class TestContract",
            "@unittest.skip('foreign')\nclass TestContract",
        ),
    )
    .unwrap();
    let second = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Update File: test_contract.py\n@@\n-EXPECTED = 2\n+EXPECTED = 3\n*** End Patch").await;
    tracker.track_delta("", &second);
    assert!(tracker.exact_unified_diff().is_err());
    let contents = tracker.changed_existing_contents();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].0, "test_sibling.py");
    assert_eq!(tracker.uncertain_content_paths(), ["test_contract.py"]);
    let partial = tracker.get_unified_diff().unwrap();
    assert!(partial.contains("+    assert 1 == 1"));
    assert!(!partial.contains("foreign"));
    tracker.record_unknown_mutation();
    assert!(tracker.changed_existing_contents().is_empty());
    assert!(
        tracker
            .uncertain_content_paths()
            .contains(&"test_sibling.py".to_string())
    );
}

#[tokio::test]
async fn absence_survives_an_add_delete_cycle_for_continuity() {
    for foreign in [false, true] {
        let dir = tempdir().unwrap();
        let mut tracker = tracker_with_root(dir.path());
        for patch in [
            "*** Begin Patch\n*** Add File: new.py\n+owned\n*** End Patch",
            "*** Begin Patch\n*** Delete File: new.py\n*** End Patch",
        ] {
            let delta = apply_verified_patch(dir.path(), patch).await;
            tracker.track_delta("", &delta);
        }
        if foreign {
            fs::write(dir.path().join("new.py"), "foreign\n").unwrap();
        }
        let delta = apply_verified_patch(
            dir.path(),
            if foreign {
                "*** Begin Patch\n*** Update File: new.py\n@@\n-foreign\n+new owned\n*** End Patch"
            } else {
                "*** Begin Patch\n*** Add File: new.py\n+new owned\n*** End Patch"
            },
        )
        .await;
        tracker.track_delta("", &delta);
        assert_eq!(tracker.exact_unified_diff().is_err(), foreign);
        assert!(tracker.changed_existing_contents().is_empty());
        assert_eq!(tracker.uncertain_content_paths().is_empty(), !foreign);
    }
}

#[tokio::test]
async fn audit107_patch_preimages_check_existence_and_both_move_endpoints() {
    for operation in [
        "delete",
        "overwrite",
        "move_source",
        "move_destination",
        "recreate",
    ] {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("source"), "before\n").unwrap();
        fs::write(dir.path().join("destination"), "original\n").unwrap();
        let mut tracker = tracker_with_root(dir.path());
        let seed = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Update File: source\n@@\n-before\n+owned\n*** Update File: destination\n@@\n-original\n+owned destination\n*** End Patch").await;
        tracker.track_delta("", &seed);
        let patch = match operation {
            "delete" => "*** Delete File: source",
            "overwrite" | "recreate" => "*** Update File: source\n@@\n-foreign\n+next",
            _ => "*** Update File: source\n*** Move to: destination",
        };
        if operation == "recreate" {
            let delete = apply_verified_patch(
                dir.path(),
                "*** Begin Patch\n*** Delete File: source\n*** End Patch",
            )
            .await;
            tracker.track_delta("", &delete);
        }
        let foreign_path = if operation == "move_destination" {
            "destination"
        } else {
            "source"
        };
        fs::write(dir.path().join(foreign_path), "foreign\n").unwrap();
        let delta = apply_verified_patch(
            dir.path(),
            &format!("*** Begin Patch\n{patch}\n*** End Patch"),
        )
        .await;
        assert!(delta.is_exact());
        tracker.track_delta("", &delta);
        assert!(tracker.exact_unified_diff().is_err(), "{operation}");
        let partial = tracker.get_unified_diff().unwrap();
        assert!(!partial.contains("-foreign"), "{operation}: {partial}");
    }
}

#[test]
fn invalidation_reports_unknown_changes_even_without_a_published_diff() {
    for published in [false, true] {
        let mut tracker = TurnDiffTracker::new();
        if published {
            tracker.unified_diff = Some("old diff".to_string());
            assert_eq!(
                tracker.take_unified_diff_if_changed(),
                Some("old diff".to_string())
            );
        }
        assert_eq!(tracker.take_invalidation_warning(), None);
        assert!(tracker.changed_paths().is_empty());
        assert!(!tracker.has_untracked_changes());
        tracker.record_unknown_mutation();
        // An invalidated diff has no listed paths, yet must not read as "no changes".
        assert!(tracker.changed_paths().is_empty());
        assert!(tracker.has_untracked_changes());
        assert_eq!(
            tracker.take_unified_diff_if_changed(),
            published.then(String::new)
        );
        let warning = tracker.take_invalidation_warning().unwrap();
        assert!(warning.starts_with("The turn diff is unavailable"));
        assert!(warning.contains("unknown_mutation"));
        assert!(warning.contains("Do not claim that no files changed"));
        assert_eq!(tracker.take_invalidation_warning(), None);
        assert_eq!(tracker.take_unified_diff_if_changed(), None);
    }
}

#[test]
fn invalidation_retains_first_command_cause_in_warning() {
    let mut tracker = TurnDiffTracker::new();
    let command = vec!["python".to_string(), "λ".repeat(300)];
    tracker.record_command_invalidation_cause(
        Some("call-1"),
        "exec_command",
        &command,
        &CommandMutation::Uncertain,
    );
    tracker.record_exec_command_end_with_mutation_at(
        &command,
        1,
        false,
        "",
        None,
        CommandMutation::Uncertain,
    );
    let first = tracker
        .first_invalidation_cause
        .clone()
        .expect("first cause");
    assert_eq!(first["call_id"], "call-1");
    assert_eq!(first["tool"], "exec_command");
    assert_eq!(first["mutation_class"], "uncertain");
    assert_eq!(
        first["command_prefix"].as_str().unwrap().chars().count(),
        240
    );
    tracker.record_unknown_mutation();
    assert_eq!(tracker.first_invalidation_cause.as_ref(), Some(&first));
    let warning = tracker.take_invalidation_warning().unwrap();
    assert!(warning.contains(&first.to_string()));
    assert!(tracker.take_invalidation_warning().is_none());
    assert_eq!(tracker.first_invalidation_cause.as_ref(), Some(&first));
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
    assert_eq!(tracker.current_mutation_revision(), 3);

    tracker.record_exec_command_end(
        &[
            "sh".into(),
            "-c".into(),
            "cargo test && touch marker".into(),
        ],
        0,
        false,
    );
    assert_eq!(tracker.current_mutation_revision(), 4);
}

#[tokio::test]
async fn formatter_checks_preserve_diff_but_writes_and_compound_scripts_invalidate() {
    let root = tempdir().unwrap();
    let delta = apply_verified_patch(
        root.path(),
        "*** Begin Patch\n*** Add File: a.rs\n+fn main() {}\n*** End Patch",
    )
    .await;
    for command in [
        vec!["C:/bin/rustfmt.exe", "--check", "a.rs"],
        vec!["/usr/bin/cargo", "fmt", "--check"],
        vec!["prettier", "--list-different", "a.js"],
        vec!["prettier", "-l", "a.js"],
        vec!["pwsh", "-Command", "prettier --check a.js"],
        vec!["bash", "-lc", "rustfmt --check a.rs"],
    ] {
        let command = command.into_iter().map(str::to_string).collect::<Vec<_>>();
        let mut tracker = tracker_with_root(root.path());
        tracker.track_delta("", &delta);
        let before = tracker.get_unified_diff().expect("patch diff");
        assert_eq!(
            command_mutation(&command, Some(root.path())),
            CommandMutation::ReadOnly,
            "{command:?}"
        );
        tracker.record_exec_command_end(&command, 0, false);
        assert_eq!(tracker.get_unified_diff(), Some(before), "{command:?}");
    }
    // Exec completion never records a path-less classification directly: it
    // resolves it against the observed workspace and rechecks tracked paths, so
    // a rewritten file loses its exact diff while the tracker stays valid.
    fs::write(root.path().join("a.rs"), "fn main() {\n}\n").unwrap();
    for command in [
        vec!["C:/bin/rustfmt.exe", "a.rs"],
        vec!["prettier", "--write", "--check", "a.js"],
        vec![
            "pwsh",
            "-Command",
            "prettier --check a.js; Set-Content a.js changed",
        ],
        vec!["pwsh", "-Command", "cargo test\nSet-Content a.rs changed"],
        vec!["bash", "-lc", "rustfmt --check a.rs && touch marker"],
    ] {
        let command = command.into_iter().map(str::to_string).collect::<Vec<_>>();
        let mut tracker = tracker_with_root(root.path());
        tracker.track_delta("", &delta);
        let classified = command_mutation(&command, Some(root.path()));
        assert_ne!(classified, CommandMutation::ReadOnly, "{command:?}");
        let observed = if matches!(
            classified,
            CommandMutation::Uncertain | CommandMutation::KnownMutation { paths: None }
        ) {
            resolve_uncertain_command_observation(Some(true))
        } else {
            classified
        };
        let mutation = tracker
            .reconcile_command_mutation("", observed, Some(LOCAL_FS.as_ref()))
            .await;
        tracker.record_exec_command_end_with_mutation_at(
            &command,
            0,
            false,
            "",
            Some(root.path()),
            mutation,
        );
        let partial = tracker.get_unified_diff().expect("partial turn diff");
        assert!(
            partial.contains("a.rs: changed, diff unavailable"),
            "{command:?}: {partial}"
        );
        assert!(tracker.valid, "{command:?}");
    }
}

#[test]
fn repository_validation_recipes_remain_observed_instead_of_assumed_read_only() {
    for command in [
        vec!["just", "core-test-fast", "core_lib", "selected_case"],
        vec!["just", "core-gate", "config-schema-protocol"],
        vec!["cargo", "clippy"],
        vec![
            "pwsh",
            "-Command",
            "just core-test-fast core_lib selected_case",
        ],
    ] {
        let command = command.into_iter().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            command_mutation(&command, None),
            CommandMutation::Uncertain,
            "{command:?}"
        );
        assert_eq!(
            resolve_uncertain_command_observation(Some(false)),
            CommandMutation::ReadOnly
        );
        assert_eq!(
            resolve_uncertain_command_observation(Some(true)),
            CommandMutation::UnattributedWorkspaceChange
        );
    }
}

#[test]
fn just_fix_is_a_mutation_and_cannot_validate_its_own_edits() {
    let mut tracker = TurnDiffTracker::new();
    tracker.record_unknown_mutation();
    tracker.record_exec_command_end(&["cargo".into(), "check".into()], 0, false);
    assert_eq!(tracker.current_mutation_revision(), 2);

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
    assert_eq!(tracker.current_mutation_revision(), 3);
}

#[test]
fn read_only_shell_commands_do_not_create_mutation_state() {
    let mut tracker = TurnDiffTracker::new();
    // Git reads are certified only with configured helpers suppressed, the same
    // forms approval admits (fsmonitor, external diff and textconv can write).
    let status = [
        "git",
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "status",
    ];
    let log = [
        "git",
        "-c",
        "core.fsmonitor=false",
        "log",
        "--no-ext-diff",
        "--no-textconv",
        "-1",
    ];
    tracker.record_exec_command_end(&status.map(str::to_owned), 0, false);
    tracker.record_exec_command_end(&log.map(str::to_owned), 0, false);
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
            "Get-ChildItem . | Select-Object -First 1; git --no-optional-locks -c core.fsmonitor=false status --short".into(),
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
fn shell_syntax_and_literal_arguments_have_distinct_mutation_effects() {
    for argv in [
        vec!["rg", " > ", "file"],
        vec!["rg", ">>", "file"],
        vec!["grep", "-n", ">>", "file"],
        vec!["bash", "-lc", "rg ' > ' file"],
        vec!["bash", "-lc", "rg '>>' file"],
        vec!["pwsh", "-Command", "rg '>= 3|a=>b;&{}' file"],
        vec![
            "pwsh",
            "-Command",
            "Select-String -Path file -Pattern \"a>b|c<d\"",
        ],
        vec![
            "pwsh",
            "-Command",
            "Write-Output 'it''s > literal'; Get-Content file",
        ],
        // Presentation-only config stays read-only once helpers are suppressed.
        vec![
            "git",
            "-ccolor.ui=false",
            "-c",
            "core.fsmonitor=false",
            "show",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
        ],
        vec!["git", "blame", "file"],
        vec!["git", "rev-list", "HEAD"],
    ] {
        let command = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
        let mut tracker = TurnDiffTracker::new();
        tracker.record_exec_command_end(&command, 0, false);
        assert_eq!(tracker.current_mutation_revision(), 0, "{command:?}");
    }
    for argv in [
        vec!["pwsh", "-Command", "cargo check"],
        vec!["cmd", "/c", "cargo check"],
        vec!["bash", "-lc", "cargo build >out.txt"],
        vec!["bash", "-lc", "git show HEAD >out.txt"],
        vec!["bash", "-lc", "git status; touch out.txt"],
        vec!["bash", "-lc", "cargo fmt --check; touch out.txt"],
        vec!["pwsh", "-Command", "git show HEAD >out.txt"],
        vec!["pwsh", "-Command", "rg '>' file >out.txt"],
        vec![
            "pwsh",
            "-Command",
            "Write-Output \"$(Set-Content out.txt changed)\"",
        ],
        vec!["pwsh", "-Command", "Write-Output 'unterminated >"],
        vec![
            "pwsh",
            "-Command",
            "Write-Output \"text\u{201d}; Set-Content out.txt changed #\"",
        ],
        vec![
            "pwsh",
            "-Command",
            "Write-Output @'\ntext\n'@; Set-Content out.txt changed",
        ],
        vec![
            "pwsh",
            "-Command",
            "cargo check; Set-Content out.txt changed",
        ],
        vec![
            "pwsh",
            "-Command",
            "git status; Set-Content out.txt changed",
        ],
        vec!["cmd", "/c", "git status >out.txt"],
        vec!["git", "-ccolor.ui=false", "reset", "--hard"],
        vec!["git", "branch", "-D", "old"],
        vec!["git", "tag", "new-tag"],
        vec!["git", "config", "user.name", "changed"],
    ] {
        let command = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
        let mut tracker = TurnDiffTracker::new();
        tracker.record_exec_command_end(&command, 0, false);
        assert_eq!(tracker.current_mutation_revision(), 1, "{command:?}");
    }
    assert!(command_reads_repository_history(&[
        "git".into(),
        "-ccolor.ui=false".into(),
        "show".into(),
        "HEAD".into(),
    ]));
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
            None
        ),
        CommandMutation::KnownMutation { .. }
    ));
    for command in [
        vec!["python", "edit.py"],
        vec!["node", "rewrite.js"],
        vec!["custom-codegen.exe"],
        vec!["python", "-m", "pytest"],
        vec!["cargo", "check"],
    ] {
        let command = command.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            command_mutation(&command, None),
            CommandMutation::Uncertain,
            "{command:?}"
        );
        assert!(command_may_mutate(&command), "{command:?}");
    }
    assert_eq!(CommandMutation::from(true), CommandMutation::Uncertain);
    assert_eq!(CommandMutation::from(false), CommandMutation::ReadOnly);
}

#[test]
#[cfg(windows)]
fn summary_and_turn_diff_share_command_mutation_classification() {
    for (script, read_only) in [
        ("Get-Command unknown-module-command", false),
        ("git branch", true),
        ("git branch -D topic", false),
        ("Get-FileHash README.md", true),
        (
            "Get-ChildItem | Where-Object { $_.Name -match 'test' }",
            true,
        ),
        ("Get-ChildItem | ForEach-Object { $_.Name }", true),
        ("Get-ChildItem | ForEach-Object { $_.Delete() }", false),
        ("Get-ChildItem | ForEach-Object -MemberName Delete", false),
        (
            "git -c core.fsmonitor=false diff --no-ext-diff --no-textconv --check; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }",
            true,
        ),
        (
            "git diff; if ($LASTEXITCODE -ne 0) { Remove-Item a; exit 1 }",
            false,
        ),
        ("Write-Output \"$(Remove-Item a)\"", false),
        // Read-only probes recorded as possible mutations before comma lists
        // and conditionals were lowered.
        (
            "Get-ChildItem tests -File | Select-Object Name,Length | Format-Table -AutoSize; Get-ChildItem .codex -Force -ErrorAction SilentlyContinue | Select-Object Name,Mode | Format-Table -AutoSize",
            true,
        ),
        (
            "Get-ChildItem rolloutdetect,tests -Recurse -File | Select-Object FullName,Length | Format-Table -AutoSize",
            true,
        ),
        (
            "Get-Location; Get-ChildItem -Force -Name; if (Test-Path AGENTS.md) { Get-Content AGENTS.md }",
            true,
        ),
        ("if (Test-Path a) { Remove-Item a }", false),
    ] {
        let command = ["pwsh", "-NoProfile", "-Command", script].map(str::to_owned);
        assert_eq!(
            command_mutation(&command, None) == CommandMutation::ReadOnly,
            read_only,
            "{script}"
        );
        assert_eq!(
            crate::tools::shell_output_summary::is_read_only_command(script),
            read_only,
            "{script}"
        );
    }
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
fn git_output_options_do_not_certify_read_only_commands() {
    // Git's real --output behavior independently establishes the write; a
    // subcommand label is insufficient even though ordinary diff only reads.
    let directory = tempdir().expect("tempdir");
    fs::write(directory.path().join("before"), "before\n").unwrap();
    fs::write(directory.path().join("after"), "after\n").unwrap();
    let output = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--output=changes.patch",
            "before",
            "after",
        ])
        .current_dir(directory.path())
        .output()
        .expect("run real git diff");
    assert_eq!(output.status.code(), Some(1)); // Different inputs, not a failed write.
    assert!(
        fs::read_to_string(directory.path().join("changes.patch"))
            .unwrap()
            .contains("+after")
    );
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["add", "before"])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success()
    );
    let pager = "--open-files-in-pager=echo mutated > pager-marker";
    let grep = Command::new("git")
        .args(["grep", pager, "before"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(grep.status.success());
    assert!(directory.path().join("pager-marker").is_file());
    for argv in [
        vec!["git", "grep", pager, "before"],
        vec!["git", "grep", "-Opager", "before"],
        vec!["git", "ls-remote", "--upload-pack=helper", "."],
        vec!["git", "cat-file", "--filters", "HEAD:file"],
        vec!["git", "-c", "core.fsmonitor=helper", "status"],
    ] {
        let command = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_ne!(
            command_mutation(&command, None),
            CommandMutation::ReadOnly,
            "{command:?}"
        );
        assert!(!command_is_read_only_git(&command), "{command:?}");
    }
    for subcommand in ["diff", "log", "show"] {
        // The base suppresses configured helpers, as approval requires, so each
        // flag below is what defeats the read-only classification.
        let safe = [
            "git",
            "-c",
            "core.fsmonitor=false",
            subcommand,
            "--no-ext-diff",
            "--no-textconv",
        ]
        .map(str::to_owned);
        for flags in [
            vec!["--output=changes.patch"],
            vec!["--output", "changes.patch"],
            vec!["--ext-diff"],
            vec!["--textconv"],
        ] {
            let mut command = safe.to_vec();
            command.extend(flags.into_iter().map(str::to_owned));
            assert_ne!(
                command_mutation(&command, None),
                CommandMutation::ReadOnly,
                "{command:?}"
            );
            assert!(!command_is_read_only_git(&command), "{command:?}");
            let wrapped = ["bash".to_string(), "-c".to_string(), command.join(" ")];
            assert_ne!(
                command_mutation(&wrapped, None),
                CommandMutation::ReadOnly,
                "{wrapped:?}"
            );
        }
        assert_eq!(command_mutation(&safe, None), CommandMutation::ReadOnly);
        assert!(command_is_read_only_git(&safe));
    }
}

#[tokio::test]
async fn failed_and_timed_out_known_mutators_still_invalidate_immediately() {
    for (script, exit_code, timed_out) in [
        ("Set-Content a.txt changed", 1, false),
        ("Set-Content a.txt changed", 124, true),
        (
            "Set-Content -LiteralPath a.txt -Value changed; exit 1",
            1,
            false,
        ),
        (
            "Set-Content -LiteralPath a.txt -Value changed; exit 1",
            1,
            true,
        ),
    ] {
        let dir = tempdir().expect("tempdir");
        let mut tracker = tracker_with_root(dir.path());
        let add = apply_verified_patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: a.txt\n+foo\n*** End Patch",
        )
        .await;
        tracker.track_delta("", &add);
        let command = ["powershell.exe".into(), "-Command".into(), script.into()];
        // Exec completion never records a path-less classification directly: it
        // resolves it against the observed workspace and rechecks tracked paths.
        assert!(
            matches!(
                command_mutation(&command, None),
                CommandMutation::Uncertain | CommandMutation::KnownMutation { paths: None }
            ),
            "{script}"
        );
        fs::write(dir.path().join("a.txt"), "changed\n").expect("command wrote before failing");
        let mutation = tracker
            .reconcile_command_mutation(
                "",
                resolve_uncertain_command_observation(Some(true)),
                Some(LOCAL_FS.as_ref()),
            )
            .await;
        tracker.record_exec_command_end_with_mutation_at(
            &command, exit_code, timed_out, "", None, mutation,
        );
        assert_eq!(tracker.current_mutation_revision(), 2, "{script}");
        assert!(tracker.valid, "{script}");
        assert!(tracker.has_untracked_changes(), "{script}");
        assert!(
            tracker
                .get_unified_diff()
                .expect("partial turn diff")
                .contains("a.txt: changed, diff unavailable"),
            "{script}"
        );
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
        CommandMutation::UnattributedWorkspaceChange
    ));

    let before_untracked = after_tracked;
    fs::write(dir.path().join("untracked.txt"), "new\n").expect("create untracked file");
    let after_untracked = crate::git_workspace::capture_workspace_evidence_identity(dir.path())
        .await
        .expect("untracked result");
    let observed = resolve_uncertain_command_observation(Some(before_untracked != after_untracked));
    assert!(matches!(
        observed,
        CommandMutation::UnattributedWorkspaceChange
    ));

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
    let argv = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    // Every read suppresses configured helpers, as approval requires, so the
    // global option under test is what decides each classification.
    // A different read-only repository is not evidence for the caller's cwd.
    assert!(!command_is_read_only_git(&argv(&[
        "git",
        "-C",
        ".",
        "-c",
        "core.fsmonitor=false",
        "log",
        "--no-ext-diff",
        "--no-textconv",
    ])));
    for command in [
        argv(&[
            "git",
            "-C",
            ".",
            "-c",
            "core.fsmonitor=false",
            "log",
            "--no-ext-diff",
            "--no-textconv",
        ]),
        argv(&[
            "git",
            "-c",
            "core.pager=cat",
            "-c",
            "core.fsmonitor=false",
            "log",
            "--no-ext-diff",
            "--no-textconv",
            "-1",
        ]),
        argv(&[
            "git",
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "show",
            "--no-ext-diff",
            "--no-textconv",
        ]),
    ] {
        assert!(
            !command_may_mutate(&command),
            "history read must remain non-mutating: {command:?}"
        );
        assert!(command_reads_repository_history(&command));
    }
    for command in [
        vec![
            "git",
            "-c",
            "core.pager=cat",
            "-c",
            "core.fsmonitor=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--output=changed.patch",
        ],
        vec![
            "git",
            "-C",
            ".",
            "-c",
            "core.fsmonitor=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--output=changed.patch",
        ],
        vec![
            "git",
            "-c",
            "core.pager=touch changed",
            "-c",
            "core.fsmonitor=false",
            "log",
            "--no-ext-diff",
            "--no-textconv",
        ],
    ] {
        assert!(command_may_mutate(
            &command.into_iter().map(str::to_owned).collect::<Vec<_>>()
        ));
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

/// Editing or removing an existing test can change what it asserts; adding a
/// test cannot. Completion reporting names only files that had a baseline and
/// end the turn different from it.
#[tokio::test]
async fn changed_existing_paths_exclude_added_and_restored_files() {
    let dir = tempdir().unwrap();
    for name in ["edited.txt", "deleted.txt", "renamed.txt", "restored.txt"] {
        fs::write(dir.path().join(name), "old\n").unwrap();
    }
    let mut tracker = tracker_with_root(dir.path());
    let delta = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Add File: added.txt\n+new\n*** Update File: edited.txt\n@@\n-old\n+new\n*** Delete File: deleted.txt\n*** Update File: renamed.txt\n*** Move to: destination.txt\n@@\n-old\n+new\n*** Update File: restored.txt\n@@\n-old\n+new\n*** End Patch").await;
    tracker.track_delta("", &delta);
    let restore = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: restored.txt\n@@\n-new\n+old\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &restore);
    assert_eq!(
        tracker.changed_existing_display_paths(),
        ["deleted.txt", "edited.txt", "renamed.txt"]
    );
}

#[tokio::test]
async fn uncertain_changes_recheck_only_patch_paths_and_preserve_exact_siblings() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("deleted.txt"), "old\n").unwrap();
    fs::write(dir.path().join("renamed.txt"), "old\n").unwrap();
    let mut tracker = tracker_with_root(dir.path());
    let delta = apply_verified_patch(dir.path(), "*** Begin Patch\n*** Add File: a.txt\n+one\n*** Add File: b.txt\n+two\n*** Delete File: deleted.txt\n*** Update File: renamed.txt\n*** Move to: destination.txt\n@@\n-old\n+new\n*** End Patch").await;
    tracker.track_delta("", &delta);
    let original = tracker.get_unified_diff();
    let changed_paths = tracker.changed_paths();
    assert_eq!(changed_paths.len(), 5);
    fs::write(dir.path().join("validation.log"), "unrelated").unwrap();
    let mutation = tracker
        .reconcile_command_mutation(
            "",
            CommandMutation::UnattributedWorkspaceChange,
            Some(LOCAL_FS.as_ref()),
        )
        .await;
    tracker.record_exec_command_end_with_mutation_at(&[], 0, false, "", None, mutation);
    assert_eq!(tracker.get_unified_diff(), original);
    assert_eq!(tracker.changed_paths(), changed_paths);
    assert!(!tracker.has_untracked_changes());

    fs::write(dir.path().join("b.txt"), "external edit").unwrap();
    fs::write(dir.path().join("deleted.txt"), "recreated").unwrap();
    fs::write(dir.path().join("destination.txt"), "external rename edit").unwrap();
    let mutation = tracker
        .reconcile_command_mutation("", CommandMutation::Uncertain, Some(LOCAL_FS.as_ref()))
        .await;
    tracker.record_exec_command_end_with_mutation_at(&[], 0, false, "", None, mutation);
    let partial = tracker.get_unified_diff().unwrap();
    assert!(partial.contains("+one"));
    assert!(!partial.contains("+two"));
    assert!(partial.contains("b.txt: changed, diff unavailable"));
    assert!(partial.contains("deleted.txt: changed, diff unavailable"));
    assert!(partial.contains("renamed.txt: changed, diff unavailable"));
    assert!(partial.contains("destination.txt: changed, diff unavailable"));
    assert!(tracker.valid);
    assert!(tracker.has_untracked_changes());
    assert_eq!(tracker.changed_paths(), changed_paths);
}

#[tokio::test]
async fn known_command_paths_preserve_other_diffs_until_unknown_invalidation() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());

    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+foo\n*** Add File: b.txt\n+bar\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);

    // A failing command may still change a known path. Only that path loses
    // exactness; unrelated apply_patch content remains useful.
    tracker.record_exec_command_end_at(
        &["rm".into(), "b.txt".into()],
        1,
        false,
        "",
        Some(dir.path()),
    );
    let partial = tracker.get_unified_diff().expect("partial turn diff");
    assert!(partial.contains("diff --git a/a.txt b/a.txt"));
    assert!(!partial.contains("diff --git a/b.txt b/b.txt"));
    assert!(partial.contains("b.txt: changed, diff unavailable"));
    let known_paths = vec![
        ("".into(), dir.path().join("a.txt")),
        ("".into(), dir.path().join("b.txt")),
    ];
    assert!(tracker.valid);
    assert!(tracker.has_untracked_changes());
    assert_eq!(tracker.changed_paths(), known_paths);
    tracker.invalidate();

    assert_eq!(tracker.changed_paths(), known_paths);

    assert_eq!(tracker.get_unified_diff(), None);
    tracker.record_exec_command_end_at(
        &["touch".into(), "c.txt".into()],
        0,
        false,
        "",
        Some(dir.path()),
    );
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
async fn pure_rename_preserves_paths_in_diff() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("old.txt"), "same\n").expect("seed file");

    let mut tracker = tracker_with_root(dir.path());
    let rename = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n same\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &rename);

    assert_eq!(
        tracker.get_unified_diff(),
        Some(
            "diff --git a/old.txt b/new.txt\nsimilarity index 100%\nrename from old.txt\nrename to new.txt\n"
                .to_string()
        )
    );
    assert_eq!(
        tracker.changed_paths(),
        vec![
            ("".to_string(), dir.path().join("new.txt")),
            ("".to_string(), dir.path().join("old.txt")),
        ]
    );
    assert!(!tracker.has_untracked_changes());
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
async fn repeated_updates_only_rerender_the_touched_path() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    for (index, patch) in [
        "*** Begin Patch\n*** Add File: stable.txt\n+stable\n*** End Patch",
        "*** Begin Patch\n*** Add File: hot.txt\n+value 0\n*** End Patch",
    ]
    .into_iter()
    .enumerate()
    {
        tracker.track_delta("", &apply_verified_patch(dir.path(), patch).await);
        assert_eq!(tracker.rendered_diff_count(), index + 1);
    }
    for value in 1..=3 {
        let patch = format!(
            "*** Begin Patch\n*** Update File: hot.txt\n@@\n-value {}\n+value {value}\n*** End Patch",
            value - 1,
        );
        tracker.track_delta("", &apply_verified_patch(dir.path(), &patch).await);
        assert_eq!(tracker.rendered_diff_count(), value + 2);
        let diff = tracker.get_unified_diff().expect("net diff");
        assert!(diff.contains("+stable\n"));
        assert!(diff.contains(&format!("+value {value}\n")));
        assert!(!diff.contains(&format!("+value {}\n", value - 1)));
        assert_eq!(tracker.get_unified_diff(), Some(diff));
        assert_eq!(
            tracker.rendered_diff_count(),
            value + 2,
            "reads must reuse both cached paths"
        );
    }
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

#[cfg(windows)]
#[test]
fn tracking_keys_preserve_non_utf8_native_paths() {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;
    // An unpaired surrogate is a valid native unit with no UTF-8 form; a lossy
    // fold would give both names the same U+FFFD key.
    let name = |unit: u16| {
        let mut units = "file-".encode_utf16().collect::<Vec<_>>();
        units.push(unit);
        PathBuf::from(std::ffi::OsString::from_wide(&units))
    };
    let first = name(0xD800);
    let second = name(0xD801);
    let first_key = TrackedPath::new("local", &first);
    let second_key = TrackedPath::new("local", &second);
    assert_ne!(first_key, second_key);
    assert_eq!(
        first_key.path.as_os_str().encode_wide().collect::<Vec<_>>(),
        "file-".encode_utf16().chain([0xD800]).collect::<Vec<_>>()
    );
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

#[test]
fn git_history_requires_the_executable_and_a_single_command() {
    for command in [
        vec!["python", "worker.py", "git", "log"],
        vec!["bash", "-lc", "git log -1\ncat src/lib.rs"],
        vec!["pwsh", "-Command", "git log; Get-Content src/lib.rs"],
    ] {
        let command = command.into_iter().map(str::to_string).collect::<Vec<_>>();
        assert!(!command_reads_repository_history(&command), "{command:?}");
    }
    assert!(command_may_mutate(&[
        "python".into(),
        "worker.py".into(),
        "git".into(),
        "log".into()
    ]));
    assert!(command_reads_repository_history(&[
        "git".into(),
        "log".into(),
        "-1".into()
    ]));
}

#[tokio::test]
async fn concurrent_writes_stay_unattributed_and_keep_this_turns_exact_diff() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: own.txt\n+mine\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);
    let exact = tracker
        .exact_unified_diff()
        .expect("exact")
        .expect("one edit");
    // Another session writes while an uncertain command runs. The snapshot also
    // shows this turn's own file, unchanged since its patch.
    fs::write(dir.path().join("foreign.txt"), "theirs\n").unwrap();
    let observed = BTreeSet::from(["own.txt", "foreign.txt"].map(|name| dir.path().join(name)));

    let mutation = tracker
        .attribute_observed_changes(
            "",
            Some(dir.path()),
            None,
            &observed,
            Some(LOCAL_FS.as_ref()),
        )
        .await;
    assert_eq!(
        mutation,
        CommandMutation::KnownMutation {
            paths: Some(BTreeSet::new())
        }
    );
    tracker.record_exec_command_end_with_mutation_at(
        &["python".into(), "script.py".into()],
        0,
        false,
        "",
        Some(dir.path()),
        mutation,
    );

    assert!(
        !tracker.has_untracked_changes(),
        "a foreign write must not make this diff partial"
    );
    assert_eq!(tracker.exact_unified_diff(), Ok(Some(exact)));
    let changed = tracker.changed_paths();
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(changed[0].1.ends_with("own.txt"), "{changed:?}");
    assert_eq!(
        tracker.unattributed_display_paths(),
        vec!["foreign.txt".to_string()]
    );
    assert_eq!(tracker.take_invalidation_warning(), None);
}

#[tokio::test]
async fn declared_paths_stay_owned_but_observed_rewrites_do_not() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: own.txt\n+mine\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);
    // The command declares out.txt, and own.txt no longer holds this turn's content.
    fs::write(dir.path().join("own.txt"), "rewritten\n").unwrap();
    fs::write(dir.path().join("out.txt"), "generated\n").unwrap();
    fs::write(dir.path().join("foreign.txt"), "theirs\n").unwrap();
    let observed =
        BTreeSet::from(["own.txt", "out.txt", "foreign.txt"].map(|name| dir.path().join(name)));
    let declared = BTreeSet::from([PathBuf::from("out.txt")]);

    let mutation = tracker
        .attribute_observed_changes(
            "",
            Some(dir.path()),
            Some(&declared),
            &observed,
            Some(LOCAL_FS.as_ref()),
        )
        .await;
    assert_eq!(
        mutation,
        CommandMutation::ObservedChanges {
            declared_paths: BTreeSet::from([dir.path().join("out.txt")]),
            unattributed_paths: BTreeSet::from([dir.path().join("own.txt")]),
        }
    );
    tracker.record_exec_command_end_with_mutation_at(
        &["generate".into()],
        0,
        false,
        "",
        Some(dir.path()),
        mutation,
    );

    assert_eq!(
        tracker.exact_unified_diff(),
        Err(vec!["out.txt".to_string(), "own.txt".to_string()])
    );
    assert_eq!(
        tracker.unattributed_display_paths(),
        vec!["foreign.txt".to_string(), "own.txt".to_string()]
    );
}
#[tokio::test]
async fn expanding_display_scope_rerenders_already_tracked_edits() {
    let root = tempdir().unwrap();
    let delta = apply_verified_patch(
        root.path(),
        "*** Begin Patch\n*** Add File: shared.txt\n+content\n*** End Patch\n",
    )
    .await;
    let mut tracker = TurnDiffTracker::with_environment_display_roots([(
        "alpha".into(),
        root.path().to_path_buf(),
    )]);
    tracker.track_delta("alpha", &delta);
    assert!(
        !tracker
            .take_unified_diff_if_changed()
            .unwrap()
            .contains("alpha/")
    );
    let roots = [
        ("alpha".into(), root.path().to_path_buf()),
        ("beta".into(), root.path().to_path_buf()),
    ];
    tracker.set_environment_display_roots(roots.clone());
    assert!(
        tracker
            .take_unified_diff_if_changed()
            .unwrap()
            .contains("alpha/shared.txt")
    );
    tracker.set_environment_display_roots(roots);
    assert!(tracker.take_unified_diff_if_changed().is_none());
}

#[tokio::test]
async fn remote_mode_is_unknown_even_when_a_host_namesake_exists() {
    let root = tempdir().unwrap();
    let delta = apply_verified_patch(
        root.path(),
        "*** Begin Patch\n*** Add File: shared.txt\n+content\n*** End Patch\n",
    )
    .await;
    let selected = crate::session::turn_context::TurnEnvironment::new(
        "remote".into(),
        Arc::new(
            codex_exec_server::Environment::create_for_tests(Some("ws://127.0.0.1:1".into()))
                .unwrap(),
        ),
        PathUri::from_host_native_path(root.path()).unwrap(),
        None,
    );
    let snapshot = crate::environment_selection::TurnEnvironmentSnapshot {
        turn_environments: vec![selected.clone()],
        ..Default::default()
    };
    let mut remote = TurnDiffTracker::new();
    remote.observe_environment_scope(&snapshot, &selected);
    remote.track_delta("remote", &delta);
    assert!(
        remote
            .file_mode(&TrackedPath::new("remote", &root.path().join("shared.txt")))
            .is_none()
    );
    assert!(
        remote.exact_unified_diff().is_err(),
        "unknown remote creation mode cannot be exact"
    );
    let mut local = tracker_with_root(root.path());
    local.track_delta("", &delta);
    assert!(
        local.exact_unified_diff().is_ok(),
        "local metadata remains supported"
    );
}

#[tokio::test]
async fn git_unchanged_observation_keeps_tracked_mismatches_as_source_changes() {
    let dir = tempdir().expect("tempdir");
    let mut tracker = tracker_with_root(dir.path());
    let delta = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+one\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &delta);
    assert_eq!(tracker.current_validation_revision(), 1);
    // Git saw nothing and every patched path still matches: evidence only.
    fs::write(dir.path().join("build.log"), "ignored output").unwrap();
    let mutation = tracker
        .reconcile_command_mutation("", CommandMutation::Uncertain, Some(LOCAL_FS.as_ref()))
        .await;
    tracker.record_git_unchanged_command_end_at(&[], "", None, mutation);
    assert_eq!(tracker.current_mutation_revision(), 2);
    assert_eq!(tracker.current_validation_revision(), 1);
    assert_eq!(tracker.validation_changes_since(1), Some(Vec::new()));
    // A patched path can be invisible to Git, for example an ignored file. A
    // mismatch there is still a source change.
    fs::write(dir.path().join("a.txt"), "external edit\n").unwrap();
    let mutation = tracker
        .reconcile_command_mutation("", CommandMutation::Uncertain, Some(LOCAL_FS.as_ref()))
        .await;
    tracker.record_git_unchanged_command_end_at(&[], "", None, mutation);
    assert_eq!(tracker.current_mutation_revision(), 3);
    assert_eq!(tracker.current_validation_revision(), 2);
    assert_eq!(tracker.validation_changes_since(1), None);
}

#[tokio::test]
async fn audit147_invalidated_diff_still_names_later_edit_paths() {
    for invalid in [true, false] {
        let dir = tempdir().unwrap();
        let mut tracker = tracker_with_root(dir.path());
        if invalid {
            // For example a workspace hook or a mutating finalizer.
            tracker.record_unknown_mutation();
        }
        let delta = apply_verified_patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: later.txt\n+after invalidation\n*** End Patch",
        )
        .await;
        tracker.track_delta("", &delta);
        tracker.record_exec_command_end_at(
            &["rm".into(), "gone.txt".into()],
            0,
            false,
            "",
            Some(dir.path()),
        );
        // Known edit paths stay available to completion attribution and
        // live-session publication even when the exact diff is gone.
        let changed = tracker.changed_paths();
        for name in ["later.txt", "gone.txt"] {
            assert!(
                changed.iter().any(|(_, path)| path.ends_with(name)),
                "invalid={invalid}: {changed:?}"
            );
        }
        assert_eq!(
            tracker
                .uncertain_content_paths()
                .contains(&"later.txt".to_string()),
            invalid
        );
        // An invalid diff still reports itself as wholly unavailable.
        let expected: Vec<String> = if invalid {
            Vec::new()
        } else {
            vec!["gone.txt".to_string()]
        };
        assert_eq!(tracker.exact_unified_diff(), Err(expected));
    }
}

#[tokio::test]
async fn audit147_declared_command_rewrite_is_not_relabeled_by_next_patch() {
    for declared in [true, false] {
        let dir = tempdir().unwrap();
        let mut tracker = tracker_with_root(dir.path());
        let add = apply_verified_patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: a.rs\n+fn main(){}\n*** End Patch",
        )
        .await;
        tracker.track_delta("", &add);
        // A formatter (declared) or another writer (undeclared) rewrites it.
        fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        if declared {
            tracker.record_exec_command_end_with_mutation_at(
                &["rustfmt".into(), "a.rs".into()],
                0,
                false,
                "",
                Some(dir.path()),
                CommandMutation::KnownMutation {
                    paths: Some(BTreeSet::from([PathBuf::from("a.rs")])),
                },
            );
        }
        let next = apply_verified_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a.rs\n@@\n-fn main() {}\n+fn main() { run() }\n*** End Patch",
        )
        .await;
        tracker.track_delta("", &next);
        // The diff is partial either way; only the undeclared write is
        // another writer's change.
        assert!(tracker.exact_unified_diff().is_err());
        assert_eq!(
            tracker.unattributed_display_paths().is_empty(),
            declared,
            "{:?}",
            tracker.unattributed_display_paths()
        );
    }
}

#[tokio::test]
async fn audit147_partial_to_unavailable_escalation_is_reported() {
    let dir = tempdir().unwrap();
    let mut tracker = tracker_with_root(dir.path());
    let add = apply_verified_patch(
        dir.path(),
        "*** Begin Patch\n*** Add File: a.txt\n+one\n*** End Patch",
    )
    .await;
    tracker.track_delta("", &add);
    tracker.record_exec_command_end_at(
        &["rm".into(), "a.txt".into()],
        0,
        false,
        "",
        Some(dir.path()),
    );
    let partial = tracker
        .take_invalidation_warning()
        .expect("partial diff warning");
    assert!(partial.starts_with("The turn diff is partial"), "{partial}");
    // A hook or finalizer mutation records no new cause, yet the diff is now
    // unavailable rather than partial.
    tracker.record_unknown_mutation();
    let unavailable = tracker
        .take_invalidation_warning()
        .expect("escalation warning");
    assert!(
        unavailable.starts_with("The turn diff is unavailable"),
        "{unavailable}"
    );
    // Already unavailable: nothing new to report.
    tracker.record_unknown_mutation();
    assert_eq!(tracker.take_invalidation_warning(), None);
}

#[tokio::test]
async fn audit147_display_scope_change_does_not_republish_an_invalidated_diff() {
    let root = tempdir().unwrap();
    let delta = apply_verified_patch(
        root.path(),
        "*** Begin Patch\n*** Add File: shared.txt\n+content\n*** End Patch\n",
    )
    .await;
    let roots = [
        ("alpha".to_string(), root.path().to_path_buf()),
        ("beta".to_string(), root.path().to_path_buf()),
    ];
    for invalid in [true, false] {
        let mut tracker = TurnDiffTracker::with_environment_display_roots([(
            "alpha".to_string(),
            root.path().to_path_buf(),
        )]);
        tracker.track_delta("alpha", &delta);
        assert!(tracker.take_unified_diff_if_changed().is_some());
        if invalid {
            tracker.record_unknown_mutation();
            // Invalidation withdraws the published diff.
            assert_eq!(tracker.take_unified_diff_if_changed(), Some(String::new()));
        }
        tracker.set_environment_display_roots(roots.clone());
        let republished = tracker.take_unified_diff_if_changed();
        if invalid {
            assert_eq!(republished, None, "an invalidated diff must stay withdrawn");
        } else {
            assert!(
                republished
                    .expect("rerendered diff")
                    .contains("alpha/shared.txt")
            );
        }
    }
}
