use std::path::Path;
use std::path::PathBuf;

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
fn symlink_alias_writers_share_the_destination_lock() {
    use codex_file_system::acquire_atomic_write_lock;
    use codex_file_system::atomic_write_lock_path;
    use codex_file_system::resolve_symlink_write_paths;
    use codex_file_system::write_atomically;
    use std::fs::OpenOptions;

    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("shared.toml");
    let alias = temp.path().join("config.toml");
    std::os::windows::fs::symlink_file(&target, &alias).unwrap();
    // Even a dangling alias must use the lock future direct writers will use.
    let first = acquire_atomic_write_lock(&alias).unwrap();
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(atomic_write_lock_path(&target).unwrap())
        .unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&probe).is_err());
    let other_target = target.clone();
    let writer = std::thread::spawn(move || {
        let _lock = acquire_atomic_write_lock(&other_target).unwrap();
        let paths = resolve_symlink_write_paths(&other_target).unwrap();
        let previous = std::fs::read_to_string(paths.read_path.unwrap()).unwrap();
        write_atomically(&paths.write_path, &(previous + "second = true\n")).unwrap();
    });
    write_atomically(&target, "first = true\n").unwrap();
    drop(first);
    writer.join().unwrap();
    assert!(
        std::fs::symlink_metadata(alias)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "first = true\nsecond = true\n"
    );
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
