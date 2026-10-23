#![cfg(windows)]

use codex_exec_server::LOCAL_FS;
use codex_exec_server::WalkOptions;
use codex_utils_path_uri::PathUri;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

#[tokio::test]
async fn non_unicode_directory_and_replacement_sibling_keep_distinct_walk_targets() {
    let temp = tempfile::tempdir().unwrap();
    let native_directory = temp.path().join(OsString::from_wide(&[0xd800]));
    let replacement_file = temp.path().join("�");
    let sentinel = native_directory.join("sentinel.txt");
    std::fs::create_dir(&native_directory).unwrap();
    std::fs::write(&replacement_file, b"replacement sibling").unwrap();
    std::fs::write(&sentinel, b"native directory child").unwrap();
    let root = PathUri::from_host_native_path(temp.path()).unwrap();
    let expected_directory = PathUri::from_host_native_path(&native_directory).unwrap();
    let expected_file = PathUri::from_host_native_path(&replacement_file).unwrap();
    let expected_child = PathUri::from_host_native_path(&sentinel).unwrap();
    let options = WalkOptions {
        max_depth: 4,
        max_directories: 10,
        max_entries: 20,
        follow_directory_symlinks: false,
        prune_hidden_directories: false,
        filters: Default::default(),
    };
    let outcome = LOCAL_FS.walk(&root, options.clone(), None).await.unwrap();
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(!outcome.truncated);
    let targets = outcome
        .entries
        .into_iter()
        .map(|entry| entry.path)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(targets.len(), 3);
    assert!(targets.contains(&expected_directory));
    assert!(targets.contains(&expected_file));
    assert!(targets.contains(&expected_child));
    assert_ne!(expected_directory, expected_file);
    assert_eq!(
        LOCAL_FS
            .read_file(&expected_child, None)
            .await
            .unwrap()
            .as_slice(),
        b"native directory child"
    );
    assert_eq!(
        LOCAL_FS
            .read_file(&expected_file, None)
            .await
            .unwrap()
            .as_slice(),
        b"replacement sibling"
    );

    let mut filtered = options;
    filtered.filters.include.push("*.txt".to_owned());
    let outcome = LOCAL_FS.walk(&root, filtered.clone(), None).await.unwrap();
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(!outcome.truncated);
    // Include filters traverse the directory without listing it.
    assert_eq!(outcome.entries.len(), 1);
    assert!(
        outcome
            .entries
            .iter()
            .any(|entry| entry.path == expected_child)
    );
    assert!(
        !outcome
            .entries
            .iter()
            .any(|entry| entry.path == expected_file)
    );

    filtered.filters.exclude_directories.push("�".to_owned());
    let outcome = LOCAL_FS.walk(&root, filtered, None).await.unwrap();
    assert_eq!(outcome.errors.len(), 1);
    assert_eq!(outcome.errors[0].path, expected_directory);
    assert!(
        outcome.errors[0]
            .message
            .contains("cannot apply text filters")
    );
}
