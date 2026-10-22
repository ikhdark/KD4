use crate::apply_patch;
use codex_exec_server::LOCAL_FS;
use codex_utils_path_uri::PathUri;

#[cfg(unix)]
fn link(target: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
fn link(target: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, path)
}

#[cfg(any(unix, windows))]
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "requires Windows symlink privilege or Developer Mode"
)]
async fn dangling_endpoints_reject_the_whole_patch_before_a_valid_prefix() {
    for absolute in [false, true] {
        for operation in [
            "*** Add File: link\n+first",
            "*** Add File: link\n+first\n*** Add File: target\n+second",
            "*** Add File: target\n+first\n*** Add File: link\n+second",
            "*** Update File: source\n*** Move to: link",
            "*** Add File: chain\n+first",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::write(root.join("source"), "original\n").unwrap();
            let target = if absolute {
                root.join("target")
            } else {
                "target".into()
            };
            link(&target, &root.join("link")).unwrap();
            link(std::path::Path::new("link"), &root.join("chain")).unwrap();
            let cwd = PathUri::from_host_native_path(root).unwrap();
            let patch = format!(
                "*** Begin Patch\n*** Add File: prefix\n+must not commit\n{operation}\n*** End Patch"
            );
            let failure = apply_patch(
                &patch,
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .unwrap_err();
            assert!(failure.delta.is_empty(), "{operation}");
            assert!(failure.delta.is_exact(), "{operation}");
            assert!(!root.join("prefix").exists());
            assert!(!root.join("target").exists());
            assert_eq!(
                std::fs::read_to_string(root.join("source")).unwrap(),
                "original\n"
            );
            assert_eq!(std::fs::read_link(root.join("link")).unwrap(), target);
        }
    }
}

#[cfg(any(unix, windows))]
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "requires Windows symlink privilege or Developer Mode"
)]
async fn absent_files_and_live_links_keep_their_existing_contract() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cwd = PathUri::from_host_native_path(root).unwrap();
    let created = apply_patch(
        "*** Begin Patch\n*** Add File: target\n+before\n*** End Patch",
        &cwd,
        &mut Vec::new(),
        &mut Vec::new(),
        LOCAL_FS.as_ref(),
        None,
    )
    .await
    .unwrap();
    assert!(created.is_exact());
    link(std::path::Path::new("target"), &root.join("link")).unwrap();
    let aliases = apply_patch(
        "*** Begin Patch\n*** Add File: prefix\n+never\n*** Add File: link\n+one\n*** Add File: target\n+two\n*** End Patch",
        &cwd, &mut Vec::new(), &mut Vec::new(), LOCAL_FS.as_ref(), None,
    ).await.unwrap_err();
    assert!(aliases.delta.is_empty());
    assert!(!root.join("prefix").exists());
    let updated = apply_patch(
        "*** Begin Patch\n*** Update File: link\n@@\n-before\n+after\n*** End Patch",
        &cwd,
        &mut Vec::new(),
        &mut Vec::new(),
        LOCAL_FS.as_ref(),
        None,
    )
    .await
    .unwrap();
    assert!(
        !updated.is_exact(),
        "a text delta does not describe link identity"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("target")).unwrap(),
        "after\n"
    );
    assert_eq!(
        std::fs::read_link(root.join("link")).unwrap(),
        std::path::Path::new("target")
    );
}
