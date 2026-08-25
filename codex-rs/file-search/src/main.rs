use std::io::IsTerminal;
use std::num::NonZero;
use std::path::Path;
use std::path::PathBuf;

use clap::ArgAction;
use clap::Parser;
use codex_file_search::FileMatch;
use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchResults;
use codex_file_search::run;
use serde_json::json;
use tokio::process::Command;

/// Fuzzy matches filenames under a directory.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Whether to output results in JSON format.
    #[clap(long, default_value = "false")]
    json: bool,

    /// Maximum number of results to return.
    #[clap(long, short = 'l', default_value = "64")]
    limit: NonZero<usize>,

    /// Directory to search.
    #[clap(long, short = 'C')]
    cwd: Option<PathBuf>,

    /// Include matching file indices in the output.
    #[arg(long, default_value = "false")]
    compute_indices: bool,

    // Filetree traversal is I/O-bound; more than two workers has not shown a
    // meaningful benefit in practice.
    /// Number of worker threads to use.
    #[clap(long, default_value = "2")]
    threads: NonZero<usize>,

    /// Exclude patterns.
    #[arg(short, long, action = ArgAction::Append)]
    exclude: Vec<String>,

    /// Search pattern.
    pattern: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let reporter = StdioReporter {
        write_output_as_json: cli.json,
        show_indices: cli.compute_indices && std::io::stdout().is_terminal(),
    };
    run_main(cli, &reporter).await?;
    Ok(())
}

async fn run_main(
    Cli {
        pattern,
        limit,
        cwd,
        compute_indices,
        json: _,
        exclude,
        threads,
    }: Cli,
    reporter: &StdioReporter,
) -> anyhow::Result<()> {
    let search_directory = match cwd {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let pattern_text = match pattern {
        Some(pattern) => pattern,
        None => {
            reporter.warn_no_search_pattern(&search_directory);

            Command::new("cmd")
                .arg("/c")
                .arg(search_directory)
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
                .await?;
            return Ok(());
        }
    };

    let FileSearchResults {
        total_match_count,
        matches,
    } = run(
        &pattern_text,
        vec![search_directory],
        FileSearchOptions {
            limit,
            exclude,
            threads,
            compute_indices,
            respect_gitignore: true,
        },
        /*cancel_flag*/ None,
    )?;
    let match_count = matches.len();

    for file_match in matches {
        reporter.report_match(&file_match);
    }
    if total_match_count > match_count {
        reporter.warn_matches_truncated(total_match_count, match_count);
    }

    Ok(())
}

struct StdioReporter {
    write_output_as_json: bool,
    show_indices: bool,
}

impl StdioReporter {
    fn report_match(&self, file_match: &FileMatch) {
        println!("{}", self.render_match(file_match));
    }

    fn render_match(&self, file_match: &FileMatch) -> String {
        if self.write_output_as_json {
            #[allow(clippy::unwrap_used)]
            return serde_json::to_string(file_match).unwrap();
        }
        if self.show_indices {
            #[allow(clippy::expect_used)]
            let indices = file_match
                .indices
                .as_ref()
                .expect("--compute-indices was specified");
            // `indices` is guaranteed to be sorted in ascending order. Instead
            // of calling `contains` for every character (which would be O(N^2)
            // in the worst-case), walk through the `indices` vector once while
            // iterating over the characters.
            let mut indices_iter = indices.iter().peekable();
            let mut rendered = String::new();

            for (i, c) in file_match.path.to_string_lossy().chars().enumerate() {
                match indices_iter.peek() {
                    Some(next) if **next == i as u32 => {
                        rendered.push_str("\x1b[1m");
                        rendered.push(c);
                        rendered.push_str("\x1b[0m");
                        indices_iter.next();
                    }
                    _ => rendered.push(c),
                }
            }
            rendered
        } else {
            file_match.path.to_string_lossy().into_owned()
        }
    }

    fn warn_matches_truncated(&self, total_match_count: usize, shown_match_count: usize) {
        if self.write_output_as_json {
            let value = json!({"matches_truncated": true});
            #[allow(clippy::unwrap_used)]
            let json = serde_json::to_string(&value).unwrap();
            println!("{json}");
        } else {
            eprintln!(
                "Warning: showing {shown_match_count} out of {total_match_count} results. Provide a more specific pattern or increase the --limit.",
            );
        }
    }

    fn warn_no_search_pattern(&self, search_directory: &Path) {
        eprintln!(
            "No search pattern specified. Showing the contents of the current directory ({}):",
            search_directory.to_string_lossy()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use codex_file_search::FileMatch;
    use codex_file_search::MatchType;
    use pretty_assertions::assert_eq;

    use clap::Parser;

    use super::Cli;
    use super::StdioReporter;

    fn file_match() -> FileMatch {
        FileMatch {
            score: 10,
            path: PathBuf::from("src/main.rs"),
            match_type: MatchType::File,
            root: PathBuf::from("repo"),
            indices: Some(vec![0, 4]),
        }
    }

    #[test]
    fn concrete_reporter_renders_plain_and_highlighted_matches() {
        let plain = StdioReporter {
            write_output_as_json: false,
            show_indices: false,
        };
        let highlighted = StdioReporter {
            write_output_as_json: false,
            show_indices: true,
        };

        assert_eq!(plain.render_match(&file_match()), "src/main.rs");
        assert_eq!(
            highlighted.render_match(&file_match()),
            "\x1b[1ms\x1b[0mrc/\x1b[1mm\x1b[0main.rs"
        );
    }

    #[test]
    fn cli_is_owned_by_the_binary_and_preserves_parser_behavior() {
        let cli = Cli::try_parse_from([
            "codex-file-search",
            "--limit",
            "7",
            "--compute-indices",
            "--exclude",
            "target",
            "needle",
        ])
        .expect("CLI arguments should parse");

        assert_eq!(cli.limit.get(), 7);
        assert!(cli.compute_indices);
        assert_eq!(cli.exclude, ["target"]);
        assert_eq!(cli.pattern.as_deref(), Some("needle"));

        let library_source = include_str!("lib.rs");
        assert!(!library_source.contains(&["mod ", "cli;"].concat()));
        assert!(!library_source.contains("pub use cli::Cli;"));
    }
}
