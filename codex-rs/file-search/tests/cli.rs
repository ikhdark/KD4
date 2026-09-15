use std::process::Command;

#[test]
fn incomplete_walk_warns_even_when_no_files_match() {
    let temp = tempfile::tempdir().expect("temp directory");
    let mut nested = temp.path().to_path_buf();
    for _ in 0..65 {
        nested.push("d");
        std::fs::create_dir(&nested).expect("create nested directory");
    }
    std::fs::write(nested.join("needle.txt"), "beyond the depth limit").expect("write needle");

    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-file-search"));
        command.arg("--cwd").arg(temp.path()).arg("needle");
        if json {
            command.arg("--json");
        }
        let output = command.output().expect("execute file-search CLI");
        assert!(output.status.success(), "{output:?}");
        if json {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("JSON warning"),
                serde_json::json!({"walk_incomplete": true})
            );
            assert!(output.stderr.is_empty(), "{output:?}");
        } else {
            assert!(output.stdout.is_empty(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("directory walk was incomplete"),
                "{output:?}"
            );
        }
    }
}

#[test]
fn match_limit_does_not_report_an_incomplete_walk() {
    let temp = tempfile::tempdir().expect("temp directory");
    for name in ["a-needle.txt", "b-needle.txt"] {
        std::fs::write(temp.path().join(name), "match").expect("write match");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_codex-file-search"))
        .arg("--cwd")
        .arg(temp.path())
        .args(["--json", "--limit", "1", "needle"])
        .output()
        .expect("execute file-search CLI");
    assert!(output.status.success(), "{output:?}");
    let rows: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)
        .expect("UTF-8 output")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON output"))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["path"], "a-needle.txt");
    assert_eq!(rows[1], serde_json::json!({"matches_truncated": true}));
    assert!(output.stderr.is_empty(), "{output:?}");
}

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
