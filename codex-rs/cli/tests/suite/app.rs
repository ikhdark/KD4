#![cfg(target_os = "windows")]

use predicates::prelude::*;

use super::codex_command;

#[test]
fn app_reports_installation_probe_failure_without_launching_installer() -> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let codex_home = temp_dir.path().join("home");
    std::fs::create_dir(&codex_home)?;
    // The test harness rejects PowerShell's -NoProfile argument. Use it as an
    // isolated failing process so this test cannot open an installed app or URL.
    std::fs::hard_link(
        std::env::current_exe()?,
        temp_dir.path().join("powershell.exe"),
    )?;
    codex_command(&codex_home)?
        .current_dir(temp_dir.path())
        .env("PATH", temp_dir.path())
        .args([
            "app",
            "--download-url",
            "https://installer.invalid/codex.exe",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "failed to check Codex Desktop installation",
        ))
        .stderr(predicate::str::contains("After installing").not())
        .stderr(predicate::str::contains("opening Windows installer").not());
    Ok(())
}
