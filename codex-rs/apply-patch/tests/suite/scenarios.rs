use codex_utils_cargo_bin::repo_root;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tempfile::tempdir;

macro_rules! apply_patch_scenario_tests {
    ($($test_name:ident => $scenario_name:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test_name() -> anyhow::Result<()> {
                run_apply_patch_scenario(&scenario_dir($scenario_name)?)
            }
        )+
    };
}

apply_patch_scenario_tests! {
    scenario_001_add_file => "001_add_file",
    scenario_002_multiple_operations => "002_multiple_operations",
    scenario_003_multiple_chunks => "003_multiple_chunks",
    scenario_004_move_to_new_directory => "004_move_to_new_directory",
    scenario_005_rejects_empty_patch => "005_rejects_empty_patch",
    scenario_006_rejects_missing_context => "006_rejects_missing_context",
    scenario_007_rejects_missing_file_delete => "007_rejects_missing_file_delete",
    scenario_008_rejects_empty_update_hunk => "008_rejects_empty_update_hunk",
    scenario_009_requires_existing_file_for_update => "009_requires_existing_file_for_update",
    scenario_010_move_overwrites_existing_destination => "010_move_overwrites_existing_destination",
    scenario_011_add_overwrites_existing_file => "011_add_overwrites_existing_file",
    scenario_012_delete_directory_fails => "012_delete_directory_fails",
    scenario_013_rejects_invalid_hunk_header => "013_rejects_invalid_hunk_header",
    scenario_014_update_file_appends_trailing_newline => "014_update_file_appends_trailing_newline",
    scenario_015_failure_after_partial_success_leaves_changes =>
        "015_failure_after_partial_success_leaves_changes",
    scenario_016_pure_addition_update_chunk => "016_pure_addition_update_chunk",
    scenario_017_whitespace_padded_hunk_header => "017_whitespace_padded_hunk_header",
    scenario_018_whitespace_padded_patch_markers => "018_whitespace_padded_patch_markers",
    scenario_019_unicode_simple => "019_unicode_simple",
    scenario_020_delete_file_success => "020_delete_file_success",
    scenario_020_whitespace_padded_patch_marker_lines =>
        "020_whitespace_padded_patch_marker_lines",
    scenario_021_update_file_deletion_only => "021_update_file_deletion_only",
    scenario_022_update_file_end_of_file_marker => "022_update_file_end_of_file_marker",
}

fn scenario_dir(scenario_name: &str) -> anyhow::Result<PathBuf> {
    Ok(repo_root()?
        .join("codex-rs")
        .join("apply-patch")
        .join("tests")
        .join("fixtures")
        .join("scenarios")
        .join(scenario_name))
}

/// Reads a scenario directory, copies the input files to a temporary directory, runs apply-patch,
/// and asserts that the final state matches the expected state exactly.
fn run_apply_patch_scenario(dir: &Path) -> anyhow::Result<()> {
    let tmp = tempdir()?;

    // Copy the input files to the temporary directory
    let input_dir = dir.join("input");
    if input_dir.is_dir() {
        copy_dir_recursive(&input_dir, tmp.path())?;
    }

    // Read the patch.txt file
    let patch = fs::read_to_string(dir.join("patch.txt"))?;

    // Run apply_patch in the temporary directory. We intentionally do not assert
    // on the exit status here; the scenarios are specified purely in terms of
    // final filesystem state, which we compare below.
    Command::new(codex_utils_cargo_bin::cargo_bin("apply_patch")?)
        .arg(patch)
        .current_dir(tmp.path())
        .output()?;

    // Assert that the final state matches the expected state exactly
    let expected_dir = dir.join("expected");
    let expected_snapshot = snapshot_dir(&expected_dir)?;
    let actual_snapshot = snapshot_dir(tmp.path())?;

    assert_eq!(
        actual_snapshot,
        expected_snapshot,
        "Scenario {} did not match expected final state",
        dir.display()
    );

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File(Vec<u8>),
    Dir,
}

fn snapshot_dir(root: &Path) -> anyhow::Result<BTreeMap<PathBuf, Entry>> {
    let mut entries = BTreeMap::new();
    if root.is_dir() {
        snapshot_dir_recursive(root, root, &mut entries)?;
    }
    Ok(entries)
}

fn snapshot_dir_recursive(
    base: &Path,
    dir: &Path,
    entries: &mut BTreeMap<PathBuf, Entry>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(stripped) = path.strip_prefix(base).ok() else {
            continue;
        };
        let rel = stripped.to_path_buf();

        // Under Buck2, files in `__srcs` are often materialized as symlinks.
        // Use `metadata()` (follows symlinks) so our fixture snapshots work
        // under both Cargo and Buck2.
        let metadata = fs::metadata(&path)?;
        if metadata.is_dir() {
            entries.insert(rel.clone(), Entry::Dir);
            snapshot_dir_recursive(base, &path, entries)?;
        } else if metadata.is_file() {
            let contents = fs::read(&path)?;
            entries.insert(rel, Entry::File(contents));
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());

        // See note in `snapshot_dir_recursive` about Buck2 symlink trees.
        let metadata = fs::metadata(&path)?;
        if metadata.is_dir() {
            fs::create_dir_all(&dest_path)?;
            copy_dir_recursive(&path, &dest_path)?;
        } else if metadata.is_file() {
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
}
