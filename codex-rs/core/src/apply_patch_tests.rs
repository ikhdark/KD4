use super::*;
use codex_exec_server::LOCAL_FS;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::Path;

use tempfile::tempdir;

async fn parse_verified_patch(cwd: &Path, patch: &str) -> ApplyPatchAction {
    let cwd = PathUri::from_host_native_path(cwd).expect("absolute test path");
    let argv = vec!["apply_patch".to_string(), patch.to_string()];
    match codex_apply_patch::maybe_parse_apply_patch_verified(
        &argv,
        &cwd,
        LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )
    .await
    {
        codex_apply_patch::MaybeApplyPatchVerified::Body(action) => action,
        other => panic!("expected verified patch body, got {other:?}"),
    }
}

#[test]
fn convert_apply_patch_maps_add_variant() {
    let tmp = tempdir().expect("tmp");
    let path = tmp.path().join("a.txt");
    let path_uri = PathUri::from_host_native_path(&path).expect("absolute test path");
    let action = ApplyPatchAction::new_add_for_test(&path_uri, "hello".to_string());

    let got = convert_apply_patch_to_protocol(&action);

    assert_eq!(
        got.get(path.as_path()),
        Some(&FileChange::Add {
            content: "hello".to_string()
        })
    );
}

#[tokio::test]
async fn convert_apply_patch_maps_delete_variant() {
    let tmp = tempdir().expect("tmp");
    let path = tmp.path().join("delete.txt");
    std::fs::write(&path, "previous content\n").expect("write source file");
    let action = parse_verified_patch(
        tmp.path(),
        "*** Begin Patch\n*** Delete File: delete.txt\n*** End Patch",
    )
    .await;

    let got = convert_apply_patch_to_protocol(&action);

    assert_eq!(
        got,
        HashMap::from([(
            path,
            FileChange::Delete {
                content: "previous content\n".to_string(),
            },
        )])
    );
}

#[tokio::test]
async fn convert_apply_patch_maps_update_variant() {
    let tmp = tempdir().expect("tmp");
    let path = tmp.path().join("update.txt");
    std::fs::write(&path, "before\n").expect("write source file");
    let action = parse_verified_patch(
        tmp.path(),
        "*** Begin Patch\n*** Update File: update.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .await;

    let got = convert_apply_patch_to_protocol(&action);

    assert_eq!(
        got,
        HashMap::from([(
            path,
            FileChange::Update {
                unified_diff: "@@ -1 +1 @@\n-before\n+after\n".to_string(),
                move_path: None,
            },
        )])
    );
}

#[tokio::test]
async fn convert_apply_patch_maps_update_with_move_variant() {
    let tmp = tempdir().expect("tmp");
    let source_path = tmp.path().join("source.txt");
    let destination_path = tmp.path().join("destination.txt");
    std::fs::write(&source_path, "before\n").expect("write source file");
    let action = parse_verified_patch(
        tmp.path(),
        "*** Begin Patch\n*** Update File: source.txt\n*** Move to: destination.txt\n@@\n-before\n+after\n*** End Patch",
    )
    .await;

    let got = convert_apply_patch_to_protocol(&action);

    assert_eq!(
        got,
        HashMap::from([(
            source_path,
            FileChange::Update {
                unified_diff: "@@ -1 +1 @@\n-before\n+after\n".to_string(),
                move_path: Some(destination_path),
            },
        )])
    );
}
