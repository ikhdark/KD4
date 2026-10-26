use std::path::PathBuf;

use clap::ArgAction;
use clap::ArgGroup;
use clap::Parser;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::find_codex_home;

#[derive(Debug, Parser)]
#[command(group(
    ArgGroup::new("sandbox_user")
        .required(true)
        .args(["user", "current_user"])
))]
pub(crate) struct SandboxSetupCommand {
    /// Set up the elevated Windows sandbox.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxSetupLevel {
    Elevated,
}

impl SandboxSetupCommand {
    fn setup_level(&self) -> anyhow::Result<SandboxSetupLevel> {
        if self.elevated_sandbox_level {
            Ok(SandboxSetupLevel::Elevated)
        } else {
            anyhow::bail!("`codex sandbox setup` currently requires --elevated");
        }
    }
}

pub(crate) async fn run(cmd: SandboxSetupCommand) -> anyhow::Result<()> {
    match cmd.setup_level()? {
        SandboxSetupLevel::Elevated => run_elevated(cmd).await,
    }
}

async fn run_elevated(cmd: SandboxSetupCommand) -> anyhow::Result<()> {
    let identity = resolve_sandbox_setup_identity(&cmd)?;

    codex_core::windows_sandbox::run_elevated_provisioning_setup(
        identity.codex_home.as_path(),
        identity.real_user.as_str(),
    )?;
    ConfigEditsBuilder::new(identity.codex_home.as_path())
        .set_windows_sandbox_mode("elevated")
        .apply()
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "sandbox provisioning succeeded, but failed to persist elevated sandbox config: {err}"
            )
        })?;

    println!(
        "Windows elevated sandbox setup completed for {} at {}.",
        identity.real_user,
        identity.codex_home.display()
    );
    Ok(())
}

struct SandboxSetupIdentity {
    real_user: String,
    codex_home: PathBuf,
}

fn resolve_sandbox_setup_identity(
    cmd: &SandboxSetupCommand,
) -> anyhow::Result<SandboxSetupIdentity> {
    if cmd.current_user {
        let real_user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .map_err(|err| {
                anyhow::anyhow!("failed to determine current user from environment: {err}")
            })?;
        let codex_home = match cmd.codex_home.clone() {
            Some(codex_home) => codex_home,
            None => find_codex_home()?.to_path_buf(),
        };
        return Ok(SandboxSetupIdentity {
            real_user,
            codex_home,
        });
    }

    let real_user = cmd
        .user
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--user or --current-user is required"))?;
    let codex_home = cmd
        .codex_home
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--codex-home is required with --user"))?;
    Ok(SandboxSetupIdentity {
        real_user,
        codex_home,
    })
}

