use clap::Parser;
use codex_file_search::source_search::SourceSearchCli;
use codex_file_search::source_search::run_source_search_cli;

fn main() -> anyhow::Result<()> {
    run_source_search_cli(SourceSearchCli::parse())
}
