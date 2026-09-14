use super::*;
use codex_git_utils::GitBaselineChange;
use codex_git_utils::GitBaselineChangeStatus;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::TempDir;

#[test]
fn render_workspace_diff_file_bounds_large_diff() {
    let mut diff = GitBaselineDiff {
        changes: vec![GitBaselineChange {
            status: GitBaselineChangeStatus::Modified,
            path: "MEMORY.md".to_string(),
        }],
        unified_diff: "a".repeat(crate::workspace_diff::MAX_BYTES),
    };

    let ascii = render_workspace_diff_file(&diff);
    let body = ascii.split_once("```diff\n").unwrap().1;
    let cutoff = body.find("\n[workspace diff truncated;").unwrap();
    // Put a multibyte character across the actual remaining artifact budget.
    diff.unified_diff = format!("{}😀tail", "a".repeat(cutoff - 1));
    let rendered = render_workspace_diff_file(&diff);

    assert!(rendered.contains("- M MEMORY.md"));
    assert!(rendered.contains("[workspace diff truncated;"));
    assert!(rendered.len() <= crate::workspace_diff::MAX_BYTES);
    assert!(!rendered.contains('😀'));
    assert!(rendered.contains("Change coverage is partial"));
}

#[tokio::test]
async fn reset_memory_workspace_baseline_removes_generated_diff() {
    let home = TempDir::new().expect("tempdir");
    let root = home.path().join("memories");
    prepare_memory_workspace(&root)
        .await
        .expect("prepare memory workspace");
    fs::write(root.join("MEMORY.md"), "memory").expect("write memory");
    write_workspace_diff(
        &root,
        &GitBaselineDiff {
            changes: vec![GitBaselineChange {
                status: GitBaselineChangeStatus::Added,
                path: "MEMORY.md".to_string(),
            }],
            unified_diff: "+memory\n".to_string(),
        },
    )
    .await
    .expect("write workspace diff");

    reset_memory_workspace_baseline(&root)
        .await
        .expect("reset baseline");

    assert!(!root.join(crate::workspace_diff::FILENAME).exists());
    let diff = memory_workspace_diff(&root)
        .await
        .expect("load workspace diff");
    assert_eq!(diff.changes, Vec::new());
}

#[tokio::test]
async fn prepare_memory_workspace_recovers_unusable_git_dir() {
    let home = TempDir::new().expect("tempdir");
    let root = home.path().join("memories");
    fs::create_dir_all(root.join(".git")).expect("create unusable git dir");
    fs::write(root.join("MEMORY.md"), "memory").expect("write memory");

    prepare_memory_workspace(&root)
        .await
        .expect("prepare memory workspace");

    let diff = memory_workspace_diff(&root)
        .await
        .expect("load workspace diff");
    assert_eq!(
        diff.changes,
        vec![GitBaselineChange {
            status: GitBaselineChangeStatus::Added,
            path: "MEMORY.md".to_string(),
        }]
    );
    prepare_memory_workspace(&root).await.unwrap();
    assert_eq!(memory_workspace_diff(&root).await.unwrap(), diff);
}

#[test]
fn render_bounds_status_list_without_claiming_complete_coverage() {
    let diff = GitBaselineDiff {
        changes: vec![GitBaselineChange {
            status: GitBaselineChangeStatus::Deleted,
            path: "x".repeat(crate::workspace_diff::MAX_BYTES),
        }],
        unified_diff: String::new(),
    };
    let rendered = render_workspace_diff_file(&diff);
    assert!(rendered.len() <= crate::workspace_diff::MAX_BYTES);
    assert!(rendered.contains("Change coverage is partial"));
    assert!(rendered.contains("including deletions"));
    assert!(!rendered.contains("- none"));
}

#[tokio::test]
async fn prepare_preserves_outstanding_changes_in_usable_baseline() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("MEMORY.md"), "old").unwrap();
    reset_memory_workspace_baseline(dir.path()).await.unwrap();
    fs::write(dir.path().join("MEMORY.md"), "new").unwrap();
    prepare_memory_workspace(dir.path()).await.unwrap();
    let diff = memory_workspace_diff(dir.path()).await.unwrap();
    assert_eq!(
        diff.changes,
        vec![GitBaselineChange {
            status: GitBaselineChangeStatus::Modified,
            path: "MEMORY.md".to_string(),
        }]
    );
    assert!(diff.unified_diff.contains("-old"));
    assert!(diff.unified_diff.contains("+new"));
}
