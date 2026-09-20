use assert_cmd::Command;
use std::fs;
use tempfile::tempdir;

fn apply_patch_command() -> anyhow::Result<Command> {
    Ok(Command::new(codex_utils_cargo_bin::cargo_bin(
        "apply_patch",
    )?))
}

#[test]
fn test_apply_patch_cli_rejects_ambiguous_matches_without_writes() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    for (original, body) in [
        ("old\nold\n", "-old\n+new"),
        ("old  \nold\t\n", "-old\n+new"),
        ("  old\n    old\n", "-old\n+new"),
        ("‘old’\n‘old’\n", "-'old'\n+new"),
        ("anchor\nold\nanchor\nold\n", "@@ anchor\n-old\n+new"),
    ] {
        fs::write(tmp.path().join("a.txt"), original)?;
        let header = if body.starts_with("@@") { "" } else { "@@\n" };
        let patch =
            format!("*** Begin Patch\n*** Update File: a.txt\n{header}{body}\n*** End Patch");
        let output = apply_patch_command()?
            .arg(patch)
            .current_dir(tmp.path())
            .assert()
            .failure();
        let stderr = String::from_utf8_lossy(&output.get_output().stderr);
        assert!(stderr.contains("Ambiguous"), "{stderr}");
        assert!(stderr.contains("context"), "{stderr}");
        assert_eq!(fs::read_to_string(tmp.path().join("a.txt"))?, original);
    }
    Ok(())
}

#[test]
fn test_apply_patch_cli_respects_indentation_sensitive_files() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    for file in ["a.py", "a.pyi", "a.yaml", "a.yml"] {
        for old in ["value = 1", "value = ‘one’"] {
            let original = format!("    {old}\n");
            fs::write(tmp.path().join(file), &original)?;
            let pattern = old.replace(['‘', '’'], "'");
            let patch = format!(
                "*** Begin Patch\n*** Update File: {file}\n@@\n-{pattern}\n+value = 2\n*** End Patch"
            );
            apply_patch_command()?
                .arg(patch)
                .current_dir(tmp.path())
                .assert()
                .failure();
            assert_eq!(fs::read_to_string(tmp.path().join(file))?, original);
        }
        let patch = format!(
            "*** Begin Patch\n*** Update File: {file}\n@@\n-    value = 'one'\n+    value = 2\n*** End Patch"
        );
        apply_patch_command()?
            .arg(patch)
            .current_dir(tmp.path())
            .assert()
            .success();
        assert_eq!(
            fs::read_to_string(tmp.path().join(file))?,
            "    value = 2\n"
        );
    }
    Ok(())
}

#[test]
fn test_apply_patch_cli_preserves_fuzzy_context_bytes() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    for (original, body, expected) in [
        (
            "‘before’\nold\n‘after’\n",
            " 'before'\n-old\n+new\n 'after'",
            "‘before’\nnew\n‘after’\n",
        ),
        (
            "anchor  \nold\ntail\t\n",
            " anchor\n-old\n+new\n tail",
            "anchor  \nnew\ntail\t\n",
        ),
        (
            "    anchor\n    old\n    tail\n",
            " anchor\n-    old\n+    new\n tail",
            "    anchor\n    new\n    tail\n",
        ),
    ] {
        fs::write(tmp.path().join("a.txt"), original)?;
        let patch = format!("*** Begin Patch\n*** Update File: a.txt\n@@\n{body}\n*** End Patch");
        apply_patch_command()?
            .arg(patch)
            .current_dir(tmp.path())
            .assert()
            .success();
        assert_eq!(fs::read_to_string(tmp.path().join("a.txt"))?, expected);
    }
    Ok(())
}

#[test]
fn test_apply_patch_cli_disambiguates_with_exact_context_and_eof() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    for (original, body, expected) in [
        ("old  \nold\n", "-old\n+new", "old  \nnew\n"),
        (
            "old\nanchor\nold\n",
            "@@ anchor\n-old\n+new",
            "old\nanchor\nnew\n",
        ),
        ("old\nold\n", "-old\n+new\n*** End of File", "old\nnew\n"),
    ] {
        fs::write(tmp.path().join("a.txt"), original)?;
        let header = if body.starts_with("@@") { "" } else { "@@\n" };
        let patch =
            format!("*** Begin Patch\n*** Update File: a.txt\n{header}{body}\n*** End Patch");
        apply_patch_command()?
            .arg(patch)
            .current_dir(tmp.path())
            .assert()
            .success();
        assert_eq!(fs::read_to_string(tmp.path().join("a.txt"))?, expected);
    }
    Ok(())
}

#[test]
fn test_apply_patch_cli_rejects_duplicate_endpoints_before_any_write() -> anyhow::Result<()> {
    for body in [
        "*** Update File: a.txt\n@@\n-before\n+after\n*** Update File: ./a.txt\n@@\n-after\n+again\n",
        "*** Add File: new.txt\n+first\n*** Add File: ./new.txt\n+second\n",
        "*** Update File: a.txt\n*** Move to: b.txt\n*** Update File: b.txt\n@@\n-before\n+after\n",
    ] {
        let tmp = tempdir()?;
        fs::write(tmp.path().join("a.txt"), "before\n")?;
        fs::write(tmp.path().join("b.txt"), "destination\n")?;
        let patch = format!(
            "*** Begin Patch\n*** Add File: prefix.txt\n+must not be written\n{body}*** End Patch"
        );
        let result = apply_patch_command()?
            .arg(patch)
            .current_dir(tmp.path())
            .assert()
            .failure();
        let stderr = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(stderr.contains("mutated more than once"), "{stderr}");
        assert!(!stderr.contains("Patch failed after applying"), "{stderr}");
        assert!(!tmp.path().join("prefix.txt").exists());
        assert!(!tmp.path().join("new.txt").exists());
        assert_eq!(fs::read_to_string(tmp.path().join("a.txt"))?, "before\n");
        assert_eq!(
            fs::read_to_string(tmp.path().join("b.txt"))?,
            "destination\n"
        );
    }
    Ok(())
}

#[test]
fn test_apply_patch_cli_updates_many_files_in_authored_order() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let mut patch = "*** Begin Patch\n".to_string();
    let mut summary = "Success. Updated the following files:\n".to_string();
    for index in (0..12).rev() {
        let name = format!("file-{index:02}.txt");
        fs::write(tmp.path().join(&name), "first\nmiddle\nlast\n")?;
        patch.push_str(&format!(
            "*** Update File: {name}\n@@\n-first\n+FIRST\n middle\n@@\n-last\n+LAST\n"
        ));
        summary.push_str(&format!("M {name}\n"));
    }
    patch.push_str("*** End Patch");
    apply_patch_command()?
        .arg(patch)
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(summary)
        .stderr("");
    for index in 0..12 {
        assert_eq!(
            fs::read_to_string(tmp.path().join(format!("file-{index:02}.txt")))?,
            "FIRST\nmiddle\nLAST\n"
        );
    }
    Ok(())
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
fn test_apply_patch_cli_preserves_fuzzy_context() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join("context.py");
    fs::write(
        &path,
        "# \u{2018}quoted\u{2019}\u{2014}context\r\nif True:\r\n    first = 1\r\n    # retained  \n    second = 2\r\n",
    )?;
    apply_patch_command()?
        .arg("*** Begin Patch\n*** Update File: context.py\n@@\n # 'quoted'-context\n if True:\n-    first = 1\n+    first = 10\n # retained\n-    second = 2\n+    second = 20\n*** End Patch")
        .current_dir(tmp.path())
        .assert()
        .success();
    assert_eq!(fs::read(&path)?, "# \u{2018}quoted\u{2019}\u{2014}context\r\nif True:\r\n    first = 10\r\n    # retained  \n    second = 20\r\n".as_bytes());
    Ok(())
}

#[test]
fn test_apply_patch_cli_requires_exact_removed_indentation() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let path = tmp.path().join("indent.py");
    fs::write(&path, "if True:\n    value = 1\n")?;
    let rejected = apply_patch_command()?
        .arg("*** Begin Patch\n*** Update File: indent.py\n@@\n if True:\n-value = 1\n+value = 2\n*** End Patch")
        .current_dir(tmp.path())
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&rejected.get_output().stderr).contains("exact indentation"));
    assert_eq!(fs::read_to_string(&path)?, "if True:\n    value = 1\n");
    apply_patch_command()?
        .arg("*** Begin Patch\n*** Update File: indent.py\n@@\n if True:\n-    value = 1\n+value = 2\n*** End Patch")
        .current_dir(tmp.path())
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&path)?, "if True:\nvalue = 2\n");
    Ok(())
}

#[test]
fn test_apply_patch_cli_reports_committed_files_on_failure() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    fs::write(tmp.path().join("first.txt"), "before\n")?;
    fs::write(tmp.path().join("second.txt"), "current\n")?;
    let rejected = apply_patch_command()?
        .arg("*** Begin Patch\n*** Update File: first.txt\n@@\n-before\n+after\n*** Update File: second.txt\n@@\n-stale\n+new\n*** End Patch")
        .current_dir(tmp.path())
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&rejected.get_output().stderr);
    assert!(
        stderr.contains("Patch failed after applying these changes:"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("M {}", tmp.path().join("first.txt").display())),
        "{stderr}"
    );
    assert!(stderr.contains("remaining changes"), "{stderr}");
    assert_eq!(fs::read_to_string(tmp.path().join("first.txt"))?, "after\n");
    assert_eq!(
        fs::read_to_string(tmp.path().join("second.txt"))?,
        "current\n"
    );
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
        stderr.contains("Patch failed after applying these changes:"),
        "{stderr}"
    );
    assert_eq!(
        stderr
            .matches(&format!("A {}", tmp.path().join("created.txt").display()))
            .count(),
        1,
        "committed files must be reported exactly once: {stderr}"
    );
    assert!(stderr.contains("do not retry the whole patch"), "{stderr}");
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

#[test]
fn test_apply_patch_cli_rejects_oversized_stdin_without_writes() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let mut patch = String::from("*** Begin Patch\n*** Add File: oversized.txt\n+");
    patch.push_str(&"x".repeat(64 * 1024 * 1024));
    patch.push_str("\n*** End Patch\n");
    let output = apply_patch_command()?
        .current_dir(tmp.path())
        .write_stdin(patch)
        .output()?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)?.contains("PATCH input exceeds the 67108864-byte limit")
    );
    assert!(!tmp.path().join("oversized.txt").exists());
    Ok(())
}
