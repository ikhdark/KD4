use clap::Args;
use clap::Subcommand;

#[derive(Debug, Args)]
pub(crate) struct ConfigCli {
    #[command(subcommand)]
    pub(crate) subcommand: self::ConfigSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigSubcommand {
    /// Explain available config.toml options in plain English.
    Explain(self::ConfigExplainArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ConfigExplainArgs {
    /// Optional option name, group, or description filter.
    #[arg(value_name = "FILTER")]
    pub(crate) filter: Option<String>,
}

impl ConfigCli {
    pub(crate) fn config_subcommand_name(&self) -> &'static str {
        match &self.subcommand {
            self::ConfigSubcommand::Explain(_) => "config explain",
        }
    }
}

pub(crate) fn run(config: self::ConfigCli) {
    let rendered = match config.subcommand {
        self::ConfigSubcommand::Explain(args) => self::render_explain(args.filter.as_deref()),
    };
    print!("{rendered}");
}

fn render_explain(filter: Option<&str>) -> String {
    codex_config::render_config_explain(filter)
}

#[cfg(test)]
#[path = "config_cmd_tests.rs"]
mod tests;
