use std::path::Path;

use anyhow::Result;

mod app;
mod app_server;
mod debug_models;
mod delete;
mod exec_server;
mod execpolicy;
mod features;
mod login;
mod marketplace_add;
mod marketplace_remove;
mod marketplace_upgrade;
mod mcp_add_remove;
mod mcp_list;
mod plugin_cli;
mod update;

/// Runs this test build's `codex` binary against an isolated home. A
/// developer-level `CODEX_SQLITE_HOME` would otherwise keep the state database
/// in the real home even though `CODEX_HOME` points at the temporary one.
fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home)
        .env("CODEX_SQLITE_HOME", codex_home);
    Ok(cmd)
}
