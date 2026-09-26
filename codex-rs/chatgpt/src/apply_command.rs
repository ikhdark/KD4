use std::path::PathBuf;

use clap::Parser;
use codex_core::config::Config;
use codex_git_utils::ApplyGitRequest;
use codex_git_utils::apply_git_patch;
use codex_utils_cli::CliConfigOverrides;

use crate::get_task::GetTaskResponse;
use crate::get_task::get_task;

/// Applies the latest diff from a Codex agent task.
#[derive(Debug, Parser)]
pub struct ApplyCommand {
    pub task_id: String,

    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,
}
pub async fn run_apply_command(
    apply_cli: ApplyCommand,
    cwd: Option<PathBuf>,
) -> anyhow::Result<()> {
    let config = Config::load_with_cli_overrides(
        apply_cli
            .config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?,
    )
    .await?;

    let task_response = get_task(&config, apply_cli.task_id).await?;
    apply_diff_from_task(task_response, cwd).await
}

pub async fn apply_diff_from_task(
    task_response: GetTaskResponse,
    cwd: Option<PathBuf>,
) -> anyhow::Result<()> {
    let Some(diff) = task_response.unified_diff() else {
        anyhow::bail!("No diff found in task");
    };
    apply_diff(diff, cwd).await
}

async fn apply_diff(diff: String, cwd: Option<PathBuf>) -> anyhow::Result<()> {
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?,
    };
    let req = ApplyGitRequest {
        cwd,
        diff,
        revert: false,
        preflight: false,
    };
    let res = tokio::task::spawn_blocking(move || apply_git_patch(&req)).await??;
    if res.exit_code != 0 {
        anyhow::bail!(
            "Git apply failed (applied={}, skipped={}, conflicts={})\nstdout:\n{}\nstderr:\n{}",
            res.applied_paths.len(),
            res.skipped_paths.len(),
            res.conflicted_paths.len(),
            res.stdout,
            res.stderr
        );
    }
    println!("Successfully applied diff");
    Ok(())
}
