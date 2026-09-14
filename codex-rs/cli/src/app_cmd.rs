use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
pub struct AppCommand {
    /// Workspace path to open in Codex Desktop.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// Override the app installer download URL (advanced).
    #[arg(long = "download-url")]
    pub download_url_override: Option<String>,
}

pub async fn run_app(cmd: AppCommand) -> anyhow::Result<()> {
    let path = std::path::absolute(&cmd.path)?;
    let workspace = std::fs::canonicalize(&path).unwrap_or(path);
    crate::desktop_app::run_app_open_or_install(workspace, cmd.download_url_override).await
}
