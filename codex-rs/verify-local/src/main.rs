use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use codex_verify_local::CargoMetadataRequest;
use codex_verify_local::CommitComparisonMode;
use codex_verify_local::PlanMode;
use codex_verify_local::PlanRequest;
use codex_verify_local::RawPath;
use codex_verify_local::RepositorySnapshot;
use codex_verify_local::Verdict;
use codex_verify_local::build_ci_decision_from_metadata;
use codex_verify_local::execute_plan;
use codex_verify_local::finalize_plan;
use codex_verify_local::load_cargo_metadata;
use codex_verify_local::plan_verification;
use codex_verify_local::render_human;
use codex_verify_local::serialize_legacy_error;
use codex_verify_local::serialize_legacy_v1;
use codex_verify_local::serialize_v2_finalized;
use codex_verify_local::verify_ci_decision_artifact;
use codex_verify_local::write_ci_decision_artifact;
use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "codex-verify-local",
    about = "Scope-locked local verification router"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(long, conflicts_with_all = ["fast", "final_"])]
    plan: bool,
    #[arg(long, conflicts_with = "final_")]
    fast: bool,
    #[arg(long = "final")]
    final_: bool,
    #[arg(long = "changed", value_name = "PATH")]
    changed: Vec<OsString>,
    #[arg(long)]
    staged: bool,
    #[arg(long)]
    all_dirty: bool,
    #[arg(long)]
    scope_start: Option<String>,
    #[arg(long, value_parser = ["current"])]
    scope: Option<String>,
    #[arg(long = "scope-add", value_name = "PATH")]
    scope_add: Vec<OsString>,
    #[arg(long)]
    scope_reset: bool,
    #[arg(long)]
    related: bool,
    #[arg(long)]
    related_tests: bool,
    #[arg(long)]
    allow_workspace: bool,
    #[arg(long)]
    isolated: bool,
    #[arg(long)]
    regen: bool,
    #[arg(long)]
    baseline: bool,
    #[arg(long)]
    retry_flakes: bool,
    #[arg(long)]
    no_cache: bool,
    #[arg(long)]
    cache_readonly: bool,
    #[arg(long)]
    json: bool,
    #[arg(long, hide = true)]
    protocol_v2: bool,
    #[arg(long, hide = true)]
    repository_root: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    CiScope(CiScopeArgs),
    VerifyCiDecision(VerifyCiDecisionArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ComparisonModeArg {
    Direct,
    PullRequest,
}

#[derive(Debug, clap::Args)]
struct CiScopeArgs {
    #[arg(long)]
    base: String,
    #[arg(long)]
    head: String,
    #[arg(long, value_enum, default_value_t = ComparisonModeArg::PullRequest)]
    comparison_mode: ComparisonModeArg,
    #[arg(long, default_value = "pull_request")]
    event: String,
    #[arg(long)]
    artifact: PathBuf,
    #[arg(long)]
    github_output: Option<PathBuf>,
    #[arg(long)]
    repository_root: Option<PathBuf>,
    #[arg(long)]
    no_cache: bool,
    #[arg(long)]
    cache_readonly: bool,
}

#[derive(Debug, clap::Args)]
struct VerifyCiDecisionArgs {
    #[arg(long)]
    artifact: PathBuf,
    #[arg(long)]
    decision_id: String,
    #[arg(long)]
    workflow: String,
    #[arg(long)]
    github_output: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    let exit_code = match run(cli) {
        Ok(code) => code,
        Err(error) => {
            let bytes = serialize_legacy_error(Verdict::ToolingError, &error, cfg!(windows));
            let _ = std::io::stdout().write_all(&bytes);
            Verdict::ToolingError.exit_code()
        }
    };
    std::process::exit(exit_code);
}

fn run(cli: Cli) -> Result<i32, String> {
    if let Some(command) = cli.command {
        return match command {
            Commands::CiScope(args) => run_ci_scope(args),
            Commands::VerifyCiDecision(args) => run_verify_ci_decision(args),
        };
    }
    let repository_root = cli
        .repository_root
        .map(Ok)
        .unwrap_or_else(find_repository_root)?;
    let changed = cli
        .changed
        .iter()
        .map(raw_path_from_cli)
        .collect::<Result<Vec<_>, _>>()?;
    let scope_add = cli
        .scope_add
        .iter()
        .map(raw_path_from_cli)
        .collect::<Result<Vec<_>, _>>()?;
    if cli.scope_start.is_some() && changed.is_empty() {
        return Err("--scope-start requires at least one --changed path".to_string());
    }
    let snapshot = if !changed.is_empty() {
        RepositorySnapshot::from_explicit_paths(&repository_root, changed.clone())
    } else if cli.scope.as_deref() == Some("current") || !scope_add.is_empty() || cli.scope_reset {
        RepositorySnapshot::from_explicit_paths(&repository_root, Vec::<RawPath>::new())
    } else {
        RepositorySnapshot::from_worktree(&repository_root)
    }
    .map_err(|error| error.to_string())?;
    let mode = if cli.final_ {
        PlanMode::Final
    } else if cli.fast {
        PlanMode::Fast
    } else {
        PlanMode::Plan
    };
    let request = PlanRequest {
        mode: Some(mode),
        changed,
        staged: cli.staged,
        all_dirty: cli.all_dirty,
        scope_current: cli.scope.as_deref() == Some("current"),
        related: cli.related,
        related_tests: cli.related_tests,
        allow_workspace: cli.allow_workspace,
        isolated: cli.isolated,
        regen: cli.regen,
        baseline: cli.baseline,
        no_cache: cli.no_cache,
        cache_readonly: cli.cache_readonly,
        retry_flakes: cli.retry_flakes,
        scope_start: cli.scope_start,
        scope_add,
        scope_reset: cli.scope_reset,
    };
    let plan = plan_verification(request, snapshot);
    let results = if mode == PlanMode::Plan || plan.verdict.is_some() {
        Vec::new()
    } else {
        execute_plan(&plan, &repository_root)
    };
    let finalized = finalize_plan(plan, results);
    let bytes = if cli.protocol_v2 {
        serialize_v2_finalized(&finalized).map_err(|error| error.to_string())?
    } else if cli.json {
        serialize_legacy_v1(&finalized, cfg!(windows)).map_err(|error| error.to_string())?
    } else {
        render_human(&finalized).into_bytes()
    };
    std::io::stdout()
        .write_all(&bytes)
        .map_err(|error| format!("failed to write verifier stdout: {error}"))?;
    Ok(finalized.exit_code)
}

fn run_ci_scope(args: CiScopeArgs) -> Result<i32, String> {
    let repository_root = args
        .repository_root
        .map(Ok)
        .unwrap_or_else(find_repository_root)?;
    let comparison = match args.comparison_mode {
        ComparisonModeArg::Direct => CommitComparisonMode::Direct,
        ComparisonModeArg::PullRequest => CommitComparisonMode::PullRequestMergeBase,
    };
    let mut snapshot =
        RepositorySnapshot::from_commit_diff(&repository_root, &args.base, &args.head, comparison)
            .unwrap_or_else(|error| {
                RepositorySnapshot::commit_diff_fallback(
                    &repository_root,
                    &args.base,
                    &args.head,
                    comparison,
                    error.to_string(),
                )
            });
    let repository_root = snapshot
        .repository_root
        .clone()
        .unwrap_or(repository_root);
    let mut metadata_request = CargoMetadataRequest::for_repository(&repository_root);
    metadata_request.no_cache = args.no_cache;
    metadata_request.cache_readonly = args.cache_readonly;
    let metadata = match load_cargo_metadata(&metadata_request) {
        Ok(result) => Some(result.metadata),
        Err(error) => {
            snapshot.complete = false;
            snapshot
                .fallback_reasons
                .push(format!("Cargo metadata fingerprint failed: {error}"));
            None
        }
    };
    let artifact = match metadata.as_ref() {
        Some(metadata) => {
            build_ci_decision_from_metadata(&repository_root, snapshot, args.event, metadata)
        }
        None => build_ci_decision_from_metadata(
            &repository_root,
            snapshot,
            args.event,
            &serde_json::Value::Null,
        ),
    }
    .map_err(|error| error.to_string())?;
    write_ci_decision_artifact(&artifact, &args.artifact).map_err(|error| error.to_string())?;
    if let Some(output) = args.github_output {
        write_github_outputs(&output, &artifact.outputs)?;
    }
    let summary = serde_json::json!({
        "decision_id": artifact.outputs.decision_id,
        "artifact_name": artifact.outputs.artifact_name,
        "full_fallback": artifact.outputs.full_fallback,
        "workflows": artifact.outputs.workflows,
        "rust_matrix": artifact.body.matrix.rust_packages,
        "rust_shards": artifact.body.matrix.rust_shards,
    });
    let mut bytes = serde_json::to_vec_pretty(&summary).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    std::io::stdout()
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    Ok(0)
}

fn run_verify_ci_decision(args: VerifyCiDecisionArgs) -> Result<i32, String> {
    let bytes = fs::read(&args.artifact)
        .map_err(|error| format!("failed to read {}: {error}", args.artifact.display()))?;
    let body = verify_ci_decision_artifact(&bytes, &args.decision_id)
        .map_err(|error| error.to_string())?;
    let workflow = body
        .workflows
        .iter()
        .find(|decision| decision.id == args.workflow)
        .ok_or_else(|| format!("decision does not define workflow {}", args.workflow))?;
    if !workflow.run {
        return Err(format!(
            "workflow {} consumed a decision that marked it skipped",
            args.workflow
        ));
    }
    if let Some(output) = args.github_output {
        append_github_output(&output, "consumed_decision_id", &args.decision_id)?;
    }
    std::io::stdout()
        .write_all(format!("{}\n", args.decision_id).as_bytes())
        .map_err(|error| error.to_string())?;
    Ok(0)
}

fn write_github_outputs(
    path: &Path,
    outputs: &codex_verify_local::CiDecisionOutputs,
) -> Result<(), String> {
    append_github_output(path, "decision_id", &outputs.decision_id)?;
    append_github_output(path, "artifact_name", &outputs.artifact_name)?;
    append_github_output(path, "full_fallback", &outputs.full_fallback.to_string())?;
    append_github_output(path, "rust_matrix", &outputs.rust_matrix_json)?;
    append_github_output(path, "rust_shards", &outputs.rust_shards_json)?;
    for workflow in &outputs.workflows {
        append_github_output(
            path,
            &format!("run_{}", workflow.id.replace('-', "_")),
            &workflow.run.to_string(),
        )?;
    }
    Ok(())
}

fn append_github_output(path: &Path, name: &str, value: &str) -> Result<(), String> {
    if value.contains(['\r', '\n']) {
        return Err(format!("GitHub output {name} contains a newline"));
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    writeln!(file, "{name}={value}")
        .and_then(|_| file.flush())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn find_repository_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    cwd.ancestors()
        .find(|candidate| {
            candidate.join("codex-rs/Cargo.toml").is_file()
                && candidate.join("scripts/verify_local_rules.toml").is_file()
        })
        .map(Path::to_path_buf)
        .ok_or_else(|| "could not locate the KD4 repository root".to_string())
}

#[cfg(unix)]
fn raw_path_from_cli(value: &OsString) -> Result<RawPath, String> {
    use std::os::unix::ffi::OsStrExt;
    let path = RawPath::new(value.as_os_str().as_bytes());
    path.validate_repository_relative()?;
    Ok(path)
}

#[cfg(windows)]
fn raw_path_from_cli(value: &OsString) -> Result<RawPath, String> {
    let text = value
        .to_str()
        .ok_or_else(|| "Windows command-line path is not valid Unicode".to_string())?;
    let normalized = text.replace('\\', "/");
    let path = RawPath::from_utf8(normalized);
    path.validate_repository_relative()?;
    Ok(path)
}
