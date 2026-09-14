use assert_cmd::Command;
use std::fs;
use tempfile::tempdir;

fn apply_patch_command() -> anyhow::Result<Command> {
    Ok(Command::new(codex_utils_cargo_bin::cargo_bin(
        "apply_patch",
    )?))
}

#[test]
fn test_apply_patch_cli_add_and_update() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let file = "cli_test.txt";
    let absolute_path = tmp.path().join(file);

    // 1) Add a file
    let add_patch = format!(
        r#"*** Begin Patch
*** Add File: {file}
+hello
*** End Patch"#
    );
    apply_patch_command()?
        .arg(add_patch)
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(format!("Success. Updated the following files:\nA {file}\n"));
    assert_eq!(fs::read_to_string(&absolute_path)?, "hello\n");

    // 2) Update the file
    let update_patch = format!(
        r#"*** Begin Patch
*** Update File: {file}
@@
-hello
+world
*** End Patch"#
    );
    apply_patch_command()?
        .arg(update_patch)
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(format!("Success. Updated the following files:\nM {file}\n"));
    assert_eq!(fs::read_to_string(&absolute_path)?, "world\n");

    Ok(())
}

#[test]
fn test_apply_patch_cli_stdin_add_and_update() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let file = "cli_test_stdin.txt";
    let absolute_path = tmp.path().join(file);

    // 1) Add a file via stdin
    let add_patch = format!(
        r#"*** Begin Patch
*** Add File: {file}
+hello
*** End Patch"#
    );
    apply_patch_command()?
        .current_dir(tmp.path())
        .write_stdin(add_patch)
        .assert()
        .success()
        .stdout(format!("Success. Updated the following files:\nA {file}\n"));
    assert_eq!(fs::read_to_string(&absolute_path)?, "hello\n");

    // 2) Update the file via stdin
    let update_patch = format!(
        r#"*** Begin Patch
*** Update File: {file}
@@
-hello
+world
*** End Patch"#
    );
    apply_patch_command()?
        .current_dir(tmp.path())
        .write_stdin(update_patch)
        .assert()
        .success()
        .stdout(format!("Success. Updated the following files:\nM {file}\n"));
    assert_eq!(fs::read_to_string(&absolute_path)?, "world\n");

    Ok(())
}

#[test]
fn test_apply_patch_cli_rejects_same_endpoint_moves() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    fs::write(tmp.path().join("a.txt"), "keep\n\n")?;
    fs::create_dir(tmp.path().join("sub"))?;
    for destination in ["a.txt", "./a.txt", "sub/../a.txt"] {
        let patch = format!(
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: {destination}\n@@\n-keep\n+changed\n*** End Patch"
        );
        let output = apply_patch_command()?
            .arg(patch)
            .current_dir(tmp.path())
            .assert()
            .failure();
        assert!(String::from_utf8_lossy(&output.get_output().stderr).contains("same path"));
        assert_eq!(fs::read_to_string(tmp.path().join("a.txt"))?, "keep\n\n");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_apply_patch_cli_rejects_move_through_symlink_alias() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    fs::write(tmp.path().join("a.txt"), "keep\n")?;
    std::os::unix::fs::symlink("a.txt", tmp.path().join("alias.txt"))?;
    apply_patch_command()?
        .arg("*** Begin Patch\n*** Update File: a.txt\n*** Move to: alias.txt\n*** End Patch")
        .current_dir(tmp.path())
        .assert()
        .failure();
    assert_eq!(fs::read_to_string(tmp.path().join("a.txt"))?, "keep\n");
    assert_eq!(fs::read_to_string(tmp.path().join("alias.txt"))?, "keep\n");
    Ok(())
}

#[test]
fn test_apply_patch_cli_preserves_and_adds_trailing_blank_lines() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    for (original, body, expected) in [
        ("old\n", "-old\n+new", "new\n"),
        ("old\n\n", "-old\n+new", "new\n\n"),
        ("old\n\n\n", "-old\n+new", "new\n\n\n"),
        ("old\n", "+", "old\n\n"),
        ("", "+", "\n"),
        ("old\n", "-old", ""),
    ] {
        fs::write(tmp.path().join("a.txt"), original)?;
        apply_patch_command()?
            .arg(format!(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n{body}\n*** End Patch"
            ))
            .current_dir(tmp.path())
            .assert()
            .success();
        assert_eq!(fs::read_to_string(tmp.path().join("a.txt"))?, expected);
    }
    Ok(())
}

#[test]
fn test_apply_patch_cli_rejects_overlapping_eof_chunks() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    fs::write(tmp.path().join("a.txt"), "a\nb\nc\n")?;
    apply_patch_command()?
        .arg("*** Begin Patch\n*** Update File: a.txt\n@@\n-c\n+first\n*** End of File\n@@\n-c\n+second\n*** End of File\n*** End Patch")
        .current_dir(tmp.path()).assert().failure();
    assert_eq!(fs::read_to_string(tmp.path().join("a.txt"))?, "a\nb\nc\n");
    Ok(())
}

#[test]
fn test_apply_patch_cli_reports_committed_prefix() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let output = apply_patch_command()?
        .arg("*** Begin Patch\n*** Add File: created.txt\n+created\n*** Delete File: missing.txt\n*** End Patch")
        .current_dir(tmp.path()).assert().failure();
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("Changes committed before the failure"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("A {}", tmp.path().join("created.txt").display())),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("created.txt"))?,
        "created\n"
    );
    assert!(!tmp.path().join("missing.txt").exists());
    Ok(())
}

#[test]
fn test_apply_patch_cli_preserves_extra_carriage_return() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    apply_patch_command()?
        .arg("*** Begin Patch\r\n*** Add File: a.txt\r\n+x\r\r\n*** End Patch")
        .current_dir(tmp.path())
        .assert()
        .success();
    assert_eq!(fs::read(tmp.path().join("a.txt"))?, b"x\r\n");
    Ok(())
}
