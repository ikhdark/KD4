use clap::Parser;
use codex_verify_local::PlanMode;
use codex_verify_local::PlanRequest;
use codex_verify_local::RawPath;
use codex_verify_local::RepositorySnapshot;
use codex_verify_local::finalize_plan;
use codex_verify_local::plan_verification;
use codex_verify_local::serialize_legacy_v1;

#[derive(Debug, Parser)]
#[command(name = "codex-verify-local")]
struct Cli {
    #[arg(long, conflicts_with_all = ["fast", "final"])]
    plan: bool,
    #[arg(long, conflicts_with = "final")]
    fast: bool,
    #[arg(long)]
    final_: bool,
    #[arg(long = "changed")]
    changed: Vec<String>,
    #[arg(long)]
    staged: bool,
    #[arg(long)]
    json: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mode = if cli.final_ {
        PlanMode::Final
    } else if cli.fast {
        PlanMode::Fast
    } else {
        PlanMode::Plan
    };
    let request = PlanRequest {
        mode: Some(mode),
        changed: cli.changed.into_iter().map(RawPath::from_utf8).collect(),
        staged: cli.staged,
        ..PlanRequest::default()
    };
    let plan = plan_verification(
        request,
        RepositorySnapshot {
            complete: true,
            ..RepositorySnapshot::default()
        },
    );
    let finalized = finalize_plan(plan, Vec::new());
    let bytes = serialize_legacy_v1(&finalized, cfg!(windows))?;
    use std::io::Write as _;
    std::io::stdout().write_all(&bytes)?;
    std::process::exit(finalized.exit_code);
}
