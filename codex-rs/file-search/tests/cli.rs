use std::process::Command;

#[test]
fn no_pattern_lists_requested_directory_without_executing_its_name() {
    let temp = tempfile::tempdir().expect("temp directory");
    let directory = temp.path().join("listing & mkdir injected");
    std::fs::create_dir(&directory).expect("create directory");
    std::fs::write(directory.join("child.txt"), "contents").expect("write child");
    std::fs::create_dir(directory.join("nested")).expect("create nested directory");
    std::fs::write(directory.join("nested").join("deeper.txt"), "nested")
        .expect("write nested child");

    let output = Command::new(env!("CARGO_BIN_EXE_codex-file-search"))
        .arg("--cwd")
        .arg(&directory)
        .current_dir(temp.path())
        .output()
        .expect("execute file-search CLI");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries: Vec<_> = stdout.lines().collect();
    entries.sort_unstable();
    assert_eq!(entries, ["child.txt", "nested"]);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("No search pattern specified"),
        "{output:?}"
    );
    assert!(!temp.path().join("injected").exists());
}

#[test]
fn no_pattern_reports_missing_directory_as_failure() {
    let temp = tempfile::tempdir().expect("temp directory");
    let missing = temp.path().join("missing");
    let output = Command::new(env!("CARGO_BIN_EXE_codex-file-search"))
        .arg("--cwd")
        .arg(&missing)
        .output()
        .expect("execute file-search CLI");

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!missing.exists());
}
