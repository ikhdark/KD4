use anyhow::Result;
use predicates::str::contains;
use tempfile::TempDir;

use super::codex_command;

#[cfg(debug_assertions)]
#[tokio::test]
async fn update_does_not_start_interactive_prompt() -> Result<()> {
    let codex_home = TempDir::new()?;

    codex_command(codex_home.path())?
        .arg("update")
        .assert()
        .failure()
        .stderr(contains("`codex update` is not available in debug builds"));

    Ok(())
}
