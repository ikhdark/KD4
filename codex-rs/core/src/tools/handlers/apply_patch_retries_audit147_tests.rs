use super::*;
use codex_exec_server::LOCAL_FS;

#[tokio::test]
async fn exact_nonprefix_residue_excludes_only_its_original_operation() {
    let root = tempfile::tempdir().unwrap();
    let cwd = PathUri::from_host_native_path(root.path()).unwrap();
    let committed = codex_apply_patch::apply_patch(
        "*** Begin Patch\n*** Add File: second\n+committed\n*** End Patch",
        &cwd, &mut Vec::new(), &mut Vec::new(), LOCAL_FS.as_ref(), None,
    ).await.unwrap();
    let original = "*** Begin Patch\n*** Add File: first\n+first\n*** Add File: second\n+committed\n*** Add File: third\n+third\n*** End Patch";
    let mut store = RetainedPatches::default();
    let receipt = store.retain(original, "local", "instance", &cwd, &committed, None).unwrap();
    assert_eq!(receipt["remaining_hunks"][0]["original_hunk"], 1);
    assert_eq!(receipt["remaining_hunks"][1]["original_hunk"], 3);
    let id = receipt["patch_id"].as_str().unwrap();
    let retry = store.prepare(&format!("*** Begin Patch\n*** Retry Patch: {id}\n*** Replace Hunk: 2\n*** Add File: third\n+corrected\n*** End Patch")).unwrap();
    assert_eq!(retry.args.hunks.len(), 2);
    let patch = render_patch(&retry.args.hunks, None);
    codex_apply_patch::apply_patch(&patch, &cwd, &mut Vec::new(), &mut Vec::new(), LOCAL_FS.as_ref(), None).await.unwrap();
    assert_eq!(std::fs::read_to_string(root.path().join("first")).unwrap(), "first\n");
    assert_eq!(std::fs::read_to_string(root.path().join("second")).unwrap(), "committed\n");
    assert_eq!(std::fs::read_to_string(root.path().join("third")).unwrap(), "corrected\n");
}
