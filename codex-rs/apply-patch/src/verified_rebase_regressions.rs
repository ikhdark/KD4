use super::*;
use codex_exec_server::LOCAL_FS;

const ANCHORED_SOURCE: &str = "fn first() {\n    one();\n}\n\nfn second() {\n    target();\n}\n";
const ANCHORED_PATCH: &str = "*** Begin Patch\n*** Update File: source.txt\n@@\n fn first() {\n-    one();\n+    uno();\n }\n@@\n-    target();\n+    goal();\n*** End Patch";
const SITE_REFUSAL: &str = "no longer matches exactly one location";

/// Verifies `patch` against `before`, lets another writer install `foreign`,
/// then applies the verified action as the runtime does.
async fn apply_after_foreign_edit(
    before: &str,
    foreign: &str,
    patch: &str,
) -> (Result<AppliedPatchDelta, ApplyPatchFailure>, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    std::fs::write(&path, before).unwrap();
    let cwd = PathUri::from_host_native_path(dir.path()).unwrap();
    let action = match verify_apply_patch_args(parse_patch(patch).unwrap(), &cwd, LOCAL_FS.as_ref(), None).await {
        MaybeApplyPatchVerified::Body(action) => action,
        other => panic!("verification failed: {other:?}"),
    };
    std::fs::write(&path, foreign).unwrap();
    let result = apply_verified_patch_with_cancellation(&action, &mut Vec::new(), &mut Vec::new(),
        LOCAL_FS.as_ref(), None, &|| false).await;
    (result, std::fs::read_to_string(&path).unwrap())
}

#[tokio::test]
async fn rebased_anchored_chunk_refuses_a_concurrent_copy_of_its_lines() {
    // The nearest `target();` after the first chunk is now the other writer's.
    let foreign = "fn first() {\n    one();\n}\n\nfn inserted() {\n    target();\n}\n\nfn second() {\n    target();\n}\n";
    let (result, after) = apply_after_foreign_edit(ANCHORED_SOURCE, foreign, ANCHORED_PATCH).await;
    let failure = result.unwrap_err();
    assert!(failure.to_string().contains(SITE_REFUSAL), "{failure}");
    assert!(failure.delta().is_exact() && failure.delta().is_empty(), "{failure:?}");
    assert_eq!(after, foreign);
}

#[tokio::test]
async fn rebased_chunk_refuses_a_stricter_tier_copy() {
    // Verification matched `alpha` only after ignoring trailing whitespace; the
    // other writer's exact copy would win the exact tier on its own.
    let before = "alpha  \n    value = 1;\nomega\n";
    let foreign = "alpha\n    value = 1;\n\nalpha  \n    value = 1;\nomega\n";
    let patch = "*** Begin Patch\n*** Update File: source.txt\n@@\n alpha\n-    value = 1;\n+    value = 2;\n*** End Patch";
    let (result, after) = apply_after_foreign_edit(before, foreign, patch).await;
    let failure = result.unwrap_err();
    assert!(failure.to_string().contains(SITE_REFUSAL), "{failure}");
    assert!(failure.delta().is_exact() && failure.delta().is_empty(), "{failure:?}");
    assert_eq!(after, foreign);
}

#[tokio::test]
async fn rebase_with_unique_sites_still_applies_around_a_disjoint_edit() {
    let foreign = format!("{ANCHORED_SOURCE}\nfn third() {{}}\n");
    let (result, after) = apply_after_foreign_edit(ANCHORED_SOURCE, &foreign, ANCHORED_PATCH).await;
    assert!(result.unwrap().is_exact());
    assert_eq!(after, "fn first() {\n    uno();\n}\n\nfn second() {\n    goal();\n}\n\nfn third() {}\n");
}

#[tokio::test]
async fn unchanged_source_applies_despite_duplicate_patterns() {
    // `target();` occurs twice, but an unchanged source is not a rebase.
    let before = format!("fn zero() {{\n    target();\n}}\n\n{ANCHORED_SOURCE}");
    let (result, after) = apply_after_foreign_edit(&before, &before, ANCHORED_PATCH).await;
    assert!(result.unwrap().is_exact());
    assert_eq!(after, "fn zero() {\n    target();\n}\n\nfn first() {\n    uno();\n}\n\nfn second() {\n    goal();\n}\n");
}
