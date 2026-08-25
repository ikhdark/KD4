use std::path::Path;
use std::path::PathBuf;

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
