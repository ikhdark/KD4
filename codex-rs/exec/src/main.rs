//! Entry-point for the `codex-exec` binary.
//!
//! Parses the standard `codex-exec` CLI options and launches the non-interactive
//! Codex agent.
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_arg0::arg0_dispatch_or_else;
use codex_exec::Cli;
use codex_exec::run_main;
use codex_utils_cli::CliConfigOverrides;

#[derive(Parser, Debug)]
struct TopCli {
    #[clap(flatten)]
    config_overrides: CliConfigOverrides,

    #[clap(flatten)]
    inner: Cli,
}

impl TopCli {
    fn parse_exec_from(args: impl IntoIterator<Item = std::ffi::OsString>) -> Cli {
        // Keep `-c` values from every level; plain clap parsing would keep only
        // the deepest subcommand's values.
        let command = CliConfigOverrides::scope_to_each_command_level(Self::command());
        let matches = command.get_matches_from(args);
        let top = Self::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
        let mut inner = top.inner;
        inner.config_overrides = CliConfigOverrides::from_command_levels(&matches);
        inner
    }
}

fn main() -> anyhow::Result<()> {
    arg0_dispatch_or_else(|arg0_paths: Arg0DispatchPaths| async move {
        run_main(TopCli::parse_exec_from(std::env::args_os()), arg0_paths).await?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
