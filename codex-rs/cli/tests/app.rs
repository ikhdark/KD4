#![cfg(target_os = "windows")]

use predicates::prelude::*;

#[test]
fn app_reports_installer_override_launch_failure() -> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let codex_home = temp_dir.path().join("home");
    std::fs::create_dir(&codex_home)?;
    // The test harness rejects PowerShell's -NoProfile argument. Use it as an
    // isolated failing process so this test cannot open an installed app or URL.
    std::fs::hard_link(
        std::env::current_exe()?,
        temp_dir.path().join("powershell.exe"),
    )?;
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command
        .current_dir(temp_dir.path())
        .env("PATH", temp_dir.path())
        .env("CODEX_HOME", codex_home)
        .args([
            "app",
            "--download-url",
            "https://installer.invalid/codex.exe",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "failed to open https://installer.invalid/codex.exe",
        ))
        .stderr(predicate::str::contains("After installing").not());
    Ok(())
}
