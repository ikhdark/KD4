//! Entry-point for the `codex-exec` binary.
//!
//! Parses the standard `codex-exec` CLI options and launches the non-interactive
//! Codex agent.
use clap::Args;
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
        // Clap propagates a global Append argument from the deepest subcommand,
        // replacing earlier values. Parse each level locally and merge in order.
        let command = Self::command()
            .mut_arg("raw_overrides", |arg| arg.global(false))
            .mut_subcommand("resume", |command| {
                CliConfigOverrides::augment_args(command)
                    .mut_arg("raw_overrides", |arg| arg.global(false))
            })
            .mut_subcommand("review", |command| {
                CliConfigOverrides::augment_args(command)
                    .mut_arg("raw_overrides", |arg| arg.global(false))
            });
        let matches = command.get_matches_from(args);
        let top = Self::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
        let mut inner = top.inner;
        inner.config_overrides = top.config_overrides;
        if let Some((_, child)) = matches.subcommand() {
            let child_overrides =
                CliConfigOverrides::from_arg_matches(child).unwrap_or_else(|error| error.exit());
            inner
                .config_overrides
                .raw_overrides
                .extend(child_overrides.raw_overrides);
        }
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
