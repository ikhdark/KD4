use std::path::Path;
use std::path::PathBuf;

// Compile the private staging fault-injection test in this existing integration
// target; the library intentionally disables its standalone unit-test target.
#[allow(dead_code)]
#[path = "../src/atomic_write.rs"]
mod atomic_write;

#[test]
fn resolution_preserves_metadata_errors() {
    let error = codex_file_system::resolve_symlink_write_paths(Path::new("invalid\0path"))
        .err()
        .expect("invalid paths must not become a replacement plan");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn resolution_rejects_cycles_and_preserves_dangling_targets() {
    use std::os::windows::fs::symlink_file;
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    symlink_file("second", &first).unwrap();
    let paths = codex_file_system::resolve_symlink_write_paths(&first).unwrap();
    assert_eq!(paths.read_path.as_ref(), Some(&second));
    assert_eq!(paths.write_path, second);
    codex_file_system::write_atomically(&paths.write_path, "target contents").unwrap();
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "target contents");
    assert_eq!(std::fs::read_link(&first).unwrap(), PathBuf::from("second"));
    std::fs::remove_file(&second).unwrap();
    symlink_file("first", &second).unwrap();
    let error = codex_file_system::resolve_symlink_write_paths(&first)
        .err()
        .expect("cycle must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("symlink cycle"));
    assert_eq!(std::fs::read_link(&first).unwrap(), PathBuf::from("second"));
    assert_eq!(std::fs::read_link(&second).unwrap(), PathBuf::from("first"));
}

#[test]
fn lock_path_is_stable_for_all_writers() {
    assert_eq!(
        codex_file_system::atomic_write_lock_path(Path::new("config.toml")).unwrap(),
        PathBuf::from(".config.toml.lock")
    );
}

#[test]
fn atomically_replaces_existing_contents() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("state.json");
    std::fs::write(&target, "old").unwrap();

    let _lock = codex_file_system::acquire_atomic_write_lock(&target).unwrap();
    codex_file_system::write_atomically(&target, "new").unwrap();

    assert_eq!(std::fs::read_to_string(target).unwrap(), "new");
}

#[test]
fn atomically_replaces_existing_contents_with_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("state.bin");
    std::fs::write(&target, b"old").unwrap();

    codex_file_system::write_bytes_atomically(&target, &[0, 1, 2, 255]).unwrap();

    assert_eq!(std::fs::read(target).unwrap(), [0, 1, 2, 255]);
}

#[test]
fn replacement_detaches_only_the_addressed_hardlink() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("target");
    let alias = directory.path().join("alias");
    std::fs::write(&destination, b"old").unwrap();
    std::fs::hard_link(&destination, &alias).unwrap();
    codex_file_system::write_bytes_atomically_without_sync(&destination, b"new").unwrap();
    assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    assert_eq!(std::fs::read(&alias).unwrap(), b"old");
}

#[test]
fn failed_replacement_preserves_destination_and_removes_staging_file() {
    let temp = tempfile::tempdir().unwrap();
    // A file cannot replace a non-empty directory, so publication fails after staging.
    let target = temp.path().join("state.bin");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("kept"), b"kept").unwrap();

    codex_file_system::write_bytes_atomically(&target, b"new")
        .expect_err("a file must not replace a directory");

    assert_eq!(std::fs::read(target.join("kept")).unwrap(), b"kept");
    let names = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(names, vec![std::ffi::OsString::from("state.bin")]);
}

#[test]
fn missing_target_can_be_created_after_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("new.toml");
    let paths = codex_file_system::resolve_symlink_write_paths(&target).unwrap();
    assert_eq!(paths.read_path.as_deref(), Some(target.as_path()));
    assert_eq!(paths.write_path, target);
    codex_file_system::write_atomically(&paths.write_path, "enabled = true").unwrap();
    assert_eq!(std::fs::read_to_string(target).unwrap(), "enabled = true");
}

#[test]
fn unsynced_write_preserves_read_only_and_missing_parent() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("readonly.txt");
    std::fs::write(&target, "before").unwrap();
    let original_permissions = std::fs::metadata(&target).unwrap().permissions();
    let mut readonly = original_permissions.clone();
    readonly.set_readonly(true);
    std::fs::set_permissions(&target, readonly).unwrap();
    let result = codex_file_system::write_bytes_atomically_without_sync(&target, b"after");
    assert!(result.is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"before");
    assert!(std::fs::metadata(&target).unwrap().permissions().readonly());
    std::fs::set_permissions(&target, original_permissions).unwrap();
    let missing = temp.path().join("missing/child.txt");
    assert!(codex_file_system::write_bytes_atomically_without_sync(&missing, b"after").is_err());
    assert!(!missing.parent().unwrap().exists());
}

#[test]
fn unsynced_write_preserves_original_without_delete_sharing() {
    use std::os::windows::fs::OpenOptionsExt;
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("open.txt");
    std::fs::write(&target, "before").unwrap();
    let _reader = std::fs::OpenOptions::new().read(true).share_mode(3).open(&target).unwrap();
    codex_file_system::write_bytes_atomically_without_sync(&target, b"after")
        .expect_err("publication must not fall back to truncation");
    assert_eq!(std::fs::read(&target).unwrap(), b"before");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn unsynced_failed_write_does_not_truncate_or_leak_staging() {
    use std::os::windows::fs::OpenOptionsExt;
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("locked.txt");
    std::fs::write(&target, "before").unwrap();
    let _reader = std::fs::OpenOptions::new().read(true).share_mode(1).open(&target).unwrap();
    let error = codex_file_system::write_bytes_atomically_without_sync(&target, b"after").unwrap_err();
    // Replacement can report access denied or sharing violation on Windows.
    assert!(matches!(error.raw_os_error(), Some(5 | 32)), "{error}");
    assert_eq!(std::fs::read(&target).unwrap(), b"before");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}
