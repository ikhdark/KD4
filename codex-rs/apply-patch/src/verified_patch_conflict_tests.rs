use super::*;
use codex_exec_server::LOCAL_FS;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

async fn verified(root: &PathUri, patch: &str) -> ApplyPatchAction {
    match verify_apply_patch_args(parse_patch(patch).unwrap(), root, LOCAL_FS.as_ref(), None).await {
        MaybeApplyPatchVerified::Body(action) => action,
        other => panic!("verification failed: {other:?}"),
    }
}

#[tokio::test]
async fn verified_delete_preserves_a_newer_foreign_preimage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    std::fs::write(&path, "verified\n").unwrap();
    let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
    let action = verified(&cwd, "*** Begin Patch\n*** Delete File: source\n*** End Patch").await;
    std::fs::write(&path, "foreign\n").unwrap();
    let failure = apply_verified_patch_with_cancellation(&action, &mut Vec::new(), &mut Vec::new(),
        LOCAL_FS.as_ref(), None, &|| false).await.unwrap_err();
    assert!(failure.delta().is_exact() && failure.delta().is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "foreign\n");
    let fresh = verified(&cwd, &action.patch).await;
    let delta = apply_verified_patch_with_cancellation(&fresh, &mut Vec::new(), &mut Vec::new(),
        LOCAL_FS.as_ref(), None, &|| false).await.unwrap();
    assert!(delta.is_exact());
    assert!(!path.exists());
}

#[tokio::test]
async fn late_add_and_move_conflicts_preserve_foreign_bytes_and_roll_back_prefix() {
    for moving in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source"), "source\n").unwrap();
        let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
        let suffix = if moving { "*** Update File: source\n*** Move to: destination\n@@\n-source\n+owned\n" }
            else { "*** Add File: destination\n+owned\n" };
        let patch = format!("*** Begin Patch\n*** Add File: prefix\n+prefix\n{suffix}*** End Patch");
        let calls = AtomicUsize::new(0);
        let conflict = || {
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                std::fs::write(dir.path().join("destination"), "foreign\n").unwrap();
            }
            false
        };
        let failure = apply_patch_with_cancellation(&patch, &cwd, &mut Vec::new(), &mut Vec::new(),
            LOCAL_FS.as_ref(), None, &conflict).await.unwrap_err();
        assert!(failure.delta().is_exact() && failure.delta().is_empty(), "{failure:?}");
        assert!(!dir.path().join("prefix").exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("source")).unwrap(), "source\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("destination")).unwrap(), "foreign\n");
    }
}

#[tokio::test]
async fn conflict_collection_prepares_and_hashes_one_immutable_source_once() {
    let dir = tempfile::tempdir().unwrap();
    let contents = "ordinary source line\n".repeat(52_000);
    std::fs::write(dir.path().join("large.txt"), &contents).unwrap();
    let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
    let mut patch = "*** Begin Patch\n*** Update File: large.txt\n".to_string();
    for index in 0..32 { patch.push_str(&format!("@@\n-missing-{index}\n+replacement-{index}\n")); }
    patch.push_str("*** End Patch");
    let args = parse_patch(&patch).unwrap();
    SOURCE_PREPARATIONS.with(|count| count.set(0));
    tests::SOURCE_GUARD_HASH_CALLS.with(|count| count.set(0));
    let error = preflight_hunks(&args.hunks, &[None], &cwd, LOCAL_FS.as_ref(), None).await.err().unwrap();
    let ApplyPatchError::MultiplePatchConflicts { mismatches, .. } = error else { panic!("missing conflicts") };
    assert_eq!(mismatches.len(), 32);
    let expected = hex::encode(Sha256::digest(contents.as_bytes()));
    assert!(mismatches.iter().enumerate().all(|(index, mismatch)|
        mismatch.chunk_ordinal == index + 1 && mismatch.current_content_sha256 == expected));
    SOURCE_PREPARATIONS.with(|count| assert_eq!(count.get(), 1));
    tests::SOURCE_GUARD_HASH_CALLS.with(|count| assert_eq!(count.get(), 1));
    assert_eq!(std::fs::read_to_string(dir.path().join("large.txt")).unwrap(), contents);
}
