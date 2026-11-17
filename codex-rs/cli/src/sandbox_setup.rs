use std::path::PathBuf;

use clap::ArgAction;
use clap::ArgGroup;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(group(
    ArgGroup::new("sandbox_user")
        .args(["user", "current_user"])
))]
pub(crate) struct SandboxSetupCommand {
    /// Legacy compatibility option; sandbox setup is unsupported.
    #[arg(long = "elevated", action = ArgAction::SetTrue)]
    elevated_sandbox_level: bool,

    /// Windows user that will run Codex after managed deployment.
    #[arg(
        long = "user",
        value_name = "USER",
        conflicts_with = "current_user",
        requires = "codex_home"
    )]
    user: Option<String>,

    /// Use the current Windows user as the Codex user.
    #[arg(
        long = "current-user",
        default_value_t = false,
        conflicts_with = "user"
    )]
    current_user: bool,

    /// CODEX_HOME for the Codex user. Required with --user.
    #[arg(long = "codex-home", value_name = "DIR")]
    codex_home: Option<PathBuf>,
}

pub(crate) async fn run(_cmd: SandboxSetupCommand) -> anyhow::Result<()> {
    anyhow::bail!("sandbox setup is unsupported: KD4 always runs with full access; no provisioning or config changes were performed")
}
