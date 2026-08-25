pub(crate) mod debug_sandbox;
mod exit_status;
pub(crate) mod login;

use clap::Args;
use clap::Parser;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_cli::ProfileV2Name;
use std::path::PathBuf;

pub use debug_sandbox::run_command_under_windows_sandbox;
pub use login::read_access_token_from_stdin;
pub use login::read_api_key_from_stdin;
pub use login::run_login_status;
pub use login::run_login_with_access_token;
pub use login::run_login_with_api_key;
pub use login::run_login_with_chatgpt;
pub use login::run_login_with_device_code;
pub use login::run_login_with_device_code_fallback_to_browser;
pub use login::run_logout;

#[derive(Debug, Default, Args)]
pub struct SandboxStateArgs {
    /// JSON value from `codex/sandbox-state-meta` to apply directly.
    #[arg(
        long = "sandbox-state-json",
        value_name = "JSON",
        conflicts_with_all = ["permissions_profile", "cwd", "include_managed_config"]
    )]
    pub sandbox_state_json: Option<String>,

    /// Add a readable root to the supplied sandbox state. Repeat for multiple roots.
    #[arg(
        long,
        requires = "sandbox_state_json",
        value_parser = parse_absolute_path
    )]
    pub sandbox_state_readable_root: Vec<AbsolutePathBuf>,

    /// Disable direct network access in the supplied sandbox state.
    #[arg(long, requires = "sandbox_state_json", default_value_t = false)]
    pub sandbox_state_disable_network: bool,
}

fn parse_absolute_path(raw: &str) -> Result<AbsolutePathBuf, String> {
    AbsolutePathBuf::relative_to_current_dir(raw)
        .map_err(|err| format!("invalid path {raw}: {err}"))
}

#[derive(Debug, Parser)]
pub struct WindowsCommand {
    #[command(flatten)]
    pub sandbox_state: SandboxStateArgs,

    /// Named permissions profile to apply from the active configuration stack.
    #[arg(
        long = "permission-profile",
        alias = "permissions-profile",
        short = 'P',
        value_name = "NAME"
    )]
    pub permissions_profile: Option<String>,

    /// Layer $CODEX_HOME/<name>.config.toml on top of the base user config.
    #[arg(long = "profile", short = 'p')]
    pub config_profile: Option<ProfileV2Name>,

    /// Working directory used for profile resolution and command execution.
    #[arg(
        short = 'C',
        long = "cd",
        value_name = "DIR",
        requires = "permissions_profile"
    )]
    pub cwd: Option<PathBuf>,

    /// Include managed requirements while resolving an explicit permissions profile.
    #[arg(
        long = "include-managed-config",
        default_value_t = false,
        requires = "permissions_profile"
    )]
    pub include_managed_config: bool,

    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    /// Full command args to run under Windows restricted token sandbox.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}
