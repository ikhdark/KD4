use super::*;
use codex_file_system::FileMutationOutcome;
use std::os::windows::fs::OpenOptionsExt;

#[tokio::test]
async fn conditional_native_mutations_refuse_stale_preimages_and_open_writers() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("file");
    let uri = PathUri::from_host_native_path(&path).unwrap();
    let fs = DirectFileSystem;
    assert!(matches!(
        fs.mutate_file_if_matches(&uri, None, Some(b"old\r\n".to_vec()), None)
            .await
            .unwrap(),
        FileMutationOutcome::Committed
    ));
    for replacement in [None, Some(b"new\r\n".to_vec())] {
        std::fs::write(&path, b"foreign\r\n").unwrap();
        assert!(matches!(
            fs.mutate_file_if_matches(&uri, Some(b"old\r\n".to_vec()), replacement.clone(), None)
                .await
                .unwrap(),
            FileMutationOutcome::Conflict
        ));
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(7)
            .open(&path)
            .unwrap();
        assert!(
            fs.mutate_file_if_matches(&uri, Some(b"foreign\r\n".to_vec()), replacement, None)
                .await
                .is_err()
        );
        drop(writer);
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign\r\n");
    }
    assert!(matches!(
        fs.mutate_file_if_matches(
            &uri,
            Some(b"foreign\r\n".to_vec()),
            Some(b"new\r\n".to_vec()),
            None
        )
        .await
        .unwrap(),
        FileMutationOutcome::Committed
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b"new\r\n");
    assert!(matches!(
        fs.mutate_file_if_matches(&uri, Some(b"new\r\n".to_vec()), None, None)
            .await
            .unwrap(),
        FileMutationOutcome::Committed
    ));
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn strict_enumeration_rejects_late_child_and_ancestor_links() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("external_only"), b"outside").unwrap();
    let parent = root.path().join("parent");
    let child = parent.join("child");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("owned"), b"inside").unwrap();
    let uri = PathUri::from_host_native_path(&child).unwrap();
    let fs = DirectFileSystem;
    assert_eq!(
        fs.read_directory_bounded_no_follow_for_walk(&uri, 10, None)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    // Retained directory handles exclude replacement during path enumeration.
    let pinned = codex_file_system::lock_directory_chain(&child).unwrap();
    assert!(std::fs::rename(&parent, root.path().join("parked")).is_err());
    assert!(std::fs::rename(&child, parent.join("parked")).is_err());
    drop(pinned);
    // A queued directory can be substituted before it is opened. The exact
    // no-follow primitive used by the walker must reject that substitution.
    std::fs::rename(&child, parent.join("parked")).unwrap();
    std::os::windows::fs::symlink_dir(outside.path(), &child).unwrap();
    assert!(
        fs.read_directory_bounded_no_follow_for_walk(&uri, 10, None)
            .await
            .is_err()
    );
    std::fs::remove_dir(&child).unwrap();
    std::fs::rename(&parent, root.path().join("parked-parent")).unwrap();
    std::os::windows::fs::symlink_dir(outside.path(), &parent).unwrap();
    // Even if the final component is ordinary, an ancestor link is rejected.
    std::fs::create_dir(outside.path().join("ordinary-child")).unwrap();
    let ancestor_uri = PathUri::from_host_native_path(parent.join("ordinary-child")).unwrap();
    assert!(
        fs.read_directory_bounded_no_follow_for_walk(&ancestor_uri, 10, None)
            .await
            .is_err()
    );
}
