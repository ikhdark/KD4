#![allow(clippy::unwrap_used, unused_imports)]

use anyhow::Context;
use assert_cmd::prelude::*;
use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_completed;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

/// While we may add an `apply-patch` subcommand to the `codex` CLI multitool
/// at some point, we must ensure that the smaller `codex-exec` CLI can still
/// emulate the `apply_patch` CLI.
#[test]
fn test_standalone_exec_cli_can_use_apply_patch() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let relative_path = "source.txt";
    let absolute_path = tmp.path().join(relative_path);
    fs::write(&absolute_path, "original content\n")?;

    Command::new(codex_utils_cargo_bin::cargo_bin("codex-exec")?)
        .arg(CODEX_CORE_APPLY_PATCH_ARG1)
        .arg(
            r#"*** Begin Patch
*** Update File: source.txt
@@
-original content
+modified by apply_patch
*** End Patch"#,
        )
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout("Success. Updated the following files:\nM source.txt\n")
        .stderr(predicates::str::is_empty());
    assert_eq!(
        fs::read_to_string(absolute_path)?,
        "modified by apply_patch\n"
    );
    Ok(())
}
