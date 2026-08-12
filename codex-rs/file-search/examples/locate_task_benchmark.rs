use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use codex_file_search::source_search::SourceSearchOptions;
use codex_file_search::source_search::search_source;
use codex_file_search::task_locator::LOCATE_TASK_MAX_FILES;
use codex_file_search::task_locator::LOCATE_TASK_MAX_SOURCE_BYTES;
use codex_file_search::task_locator::LocateTaskRequest;
use codex_file_search::task_locator::locate_task;
use serde_json::Value;
use serde_json::json;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = ".")]
    repository_root: PathBuf,

    #[arg(long, default_value = "source_owners.toml")]
    manifest: PathBuf,

    #[arg(long, default_value = "shared kd4 source index")]
    task: String,

    #[arg(long)]
    baseline_discovery_calls: Option<usize>,

    #[arg(long)]
    baseline_context_bytes: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let repository_root = args.repository_root.canonicalize()?;
    let manifest = if args.manifest.is_absolute() {
        args.manifest
    } else {
        repository_root.join(args.manifest)
    };
    let cache = tempfile::tempdir()?;
    let request = || LocateTaskRequest {
        repository_root: &repository_root,
        cache_root: cache.path(),
        manifest_path: &manifest,
        environment_id: None,
        task: &args.task,
        path_anchor: None,
        symbol_anchor: None,
        max_files: LOCATE_TASK_MAX_FILES,
        max_source_bytes: LOCATE_TASK_MAX_SOURCE_BYTES,
        force_fresh: false,
    };

    let cold_started = Instant::now();
    let cold = locate_task(&request())?;
    let cold_elapsed = cold_started.elapsed();
    let warm_started = Instant::now();
    let warm = locate_task(&request())?;
    let warm_elapsed = warm_started.elapsed();
    let source_search = benchmark_source_search()?;

    let report = json!({
        "cold_closure_indexing_ms": cold_elapsed.as_secs_f64() * 1000.0,
        "warm_locator_ms": warm_elapsed.as_secs_f64() * 1000.0,
        "files_inspected": {
            "cold": cold.files_inspected,
            "warm": warm.files_inspected,
        },
        "files_reparsed": {
            "cold": cold.files_reparsed,
            "warm": warm.files_reparsed,
        },
        "rendered_bytes": warm.rendered_bytes,
        "discovery_calls": {
            "before": args.baseline_discovery_calls,
            "after": 1,
        },
        "context_usage_bytes": {
            "before": args.baseline_context_bytes,
            "after": warm.rendered_bytes,
        },
        "source_search": source_search,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn benchmark_source_search() -> Result<Value> {
    let fixture = tempfile::tempdir()?;
    let owner = fixture.path().join("owner");
    let elsewhere = fixture.path().join("elsewhere");
    fs::create_dir(&owner)?;
    fs::create_dir(&elsewhere)?;
    for index in 0..6 {
        fs::write(
            owner.join(format!("owned_{index:02}.rs")),
            format!("pub fn owned_{index:02}() {{}}\n"),
        )?;
    }
    fs::write(owner.join("expected.rs"), "pub fn benchmark_needle() {}\n")?;
    let decoy_body = "pub fn unrelated() {}\n".repeat(128);
    for index in 0..256 {
        fs::write(elsewhere.join(format!("decoy_{index:03}.rs")), &decoy_body)?;
    }

    Ok(json!({
        "scoped_hit": run_source_search_case(fixture.path(), "benchmark_needle", true)?,
        "scoped_miss": run_source_search_case(fixture.path(), "absent_needle", true)?,
        "unscoped_hit": run_source_search_case(fixture.path(), "benchmark_needle", false)?,
        "unscoped_miss": run_source_search_case(fixture.path(), "absent_needle", false)?,
    }))
}

fn run_source_search_case(
    repository_root: &std::path::Path,
    query: &str,
    scoped: bool,
) -> Result<Value> {
    let mut options = SourceSearchOptions::new(repository_root.to_path_buf(), query.to_string());
    if scoped {
        options.roots = vec![PathBuf::from("owner")];
    }
    let output = search_source(options)?;
    Ok(json!({
        "total_us": output.diagnostics.total_micros,
        "first_match_us": output.diagnostics.first_match_micros,
        "traversal_us": output.diagnostics.traversal_micros,
        "file_scan_match_us": output.diagnostics.file_scan_match_micros,
        "projection_us": output.diagnostics.projection_micros,
        "walked": output.coverage.walked_entries,
        "ignored": output.coverage.ignored_entries,
        "scanned_files": output.coverage.files_scanned,
        "bytes_scanned": output.coverage.bytes_scanned,
        "matches": output.coverage.total_matches,
        "truncated": output.truncated,
    }))
}
