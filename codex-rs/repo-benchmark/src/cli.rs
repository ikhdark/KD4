use crate::prepare::PrepareOptions;
use crate::prepare::Prepared;
use crate::prepare::prepare;
use crate::prepare::provenance::FileIdentity;
use crate::prepare::provenance::hash_file;
use crate::schedule::Mode;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub struct Options {
    pub operation: String,
    pub mode: Mode,
    pub explicit_mode: bool,
    pub prepared: Option<PathBuf>,
    pub result: Option<PathBuf>,
    pub reference: Option<PathBuf>,
    pub fork_ref: String,
    pub attempt: Option<String>,
    pub analysis_only: bool,
}

pub fn parse(args: Vec<String>) -> Result<Options> {
    let mut result = Options {
        operation: "run".into(),
        mode: Mode::Fast,
        explicit_mode: false,
        prepared: None,
        result: None,
        reference: None,
        fork_ref: "main".into(),
        attempt: None,
        analysis_only: false,
    };
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|s| !s.starts_with('-')) {
        result.operation = args.next().context("operation")?;
    }
    ensure!(
        ["run", "prepare", "compare", "import", "rerun", "help"]
            .contains(&result.operation.as_str()),
        "unknown Repo Benchmark operation {}",
        result.operation
    );
    for_token(&mut args, &mut result)?;
    Ok(result)
}

fn for_token(args: &mut impl Iterator<Item = String>, result: &mut Options) -> Result<()> {
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-fast" | "-full" => {
                let mode = if arg == "-fast" {
                    Mode::Fast
                } else {
                    Mode::Full
                };
                ensure!(
                    !result.explicit_mode || result.mode == mode,
                    "-fast and -full cannot be combined"
                );
                result.mode = mode;
                result.explicit_mode = true;
            }
            "--prepared" => {
                result.prepared = Some(PathBuf::from(
                    args.next().context("--prepared needs a path")?,
                ))
            }
            "--result" => {
                result.result = Some(PathBuf::from(args.next().context("--result needs a path")?))
            }
            "--reference" => {
                result.reference = Some(PathBuf::from(
                    args.next()
                        .context("--reference needs a candidate checkout path")?,
                ))
            }
            "--fork-ref" => {
                result.fork_ref = args
                    .next()
                    .context("--fork-ref needs a committed revision")?
            }
            "--attempt" => {
                result.attempt = Some(
                    args.next()
                        .context("--attempt needs a scheduled attempt ID")?,
                )
            }
            "--analysis-only" => result.analysis_only = true,
            "--help" | "-h" => result.operation = "help".into(),
            _ => bail!("unknown argument {arg}; use exact mode flags -fast or -full"),
        }
    }
    Ok(())
}

fn verify_running_harness(frozen: &FileIdentity) -> Result<()> {
    frozen.verify()?;
    let executable = std::env::current_exe().context("resolve executing benchmark harness")?;
    ensure!(
        hash_file(&executable)? == frozen.sha256,
        "executing benchmark harness {} differs from the prepared harness {}; use the recorded executable or prepare again",
        executable.display(),
        frozen.path.display()
    );
    Ok(())
}

fn load_result_for_cli(path: &Path) -> Result<crate::runner::RunResult> {
    let result = crate::runner::RunResult::load(path)?;
    ensure!(
        hash_file(&result.prepared_manifest)? == result.prepared_manifest_sha256,
        "original prepared manifest changed"
    );
    let prepared = Prepared::load(&result.prepared_manifest)?;
    verify_running_harness(&prepared.harness)?;
    Ok(result)
}

pub fn run(args: Vec<String>) -> Result<()> {
    let options = parse(args)?;
    if options.operation == "help" {
        println!(
            "Repo Benchmark: one Rust benchmark, scripted then real-model execution.\n\njust repo-benchmark [-fast|-full]\njust repo-benchmark prepare [-fast|-full] [--reference CHECKOUT] [--fork-ref COMMIT]\njust repo-benchmark compare --prepared MANIFEST [-fast|-full]\njust repo-benchmark import --result RESULT\njust repo-benchmark rerun --result RESULT --attempt ID\njust repo-benchmark rerun --result RESULT --analysis-only\n\nFast: 1 live task × 3 variants. Full: 3 live tasks × 3 variants.\nAll execution is sequential and stops when finite work finishes. No performance pass/fail verdict."
        );
        return Ok(());
    }
    let path = match options.operation.as_str() {
        "prepare" | "run" | "compare" => {
            ensure!(
                !options.analysis_only && options.result.is_none() && options.attempt.is_none(),
                "result/attempt/analysis flags are only valid for rerun/import"
            );
            let manifest = if let Some(path) = options.prepared {
                ensure!(
                    options.reference.is_none() && options.fork_ref == "main",
                    "source revisions are frozen in prepared manifest"
                );
                let prepared = Prepared::load(&path)?;
                ensure!(
                    !options.explicit_mode || prepared.mode == options.mode,
                    "mode differs from frozen schedule; prepare again"
                );
                path
            } else {
                ensure!(
                    options.operation != "compare",
                    "compare requires --prepared MANIFEST"
                );
                prepare(PrepareOptions {
                    repo: std::env::current_dir()?,
                    mode: options.mode,
                    fork_ref: options.fork_ref,
                    reference_checkout: options.reference,
                })?
            };
            verify_running_harness(&Prepared::load(&manifest)?.harness)?;
            if options.operation == "prepare" {
                manifest
            } else {
                crate::runner::execute(&manifest, None, None)?
            }
        }
        "import" => {
            let path = options.result.context("import requires --result")?;
            load_result_for_cli(&path)?;
            crate::reports::import(&path)?
        }
        "rerun" => {
            ensure!(
                !options.explicit_mode && options.reference.is_none() && options.prepared.is_none(),
                "reruns preserve the original prepared configuration and schedule"
            );
            let path = options.result.context("rerun requires --result")?;
            let result = load_result_for_cli(&path)?;
            if options.analysis_only {
                ensure!(
                    options.attempt.is_none(),
                    "analysis-only rerun analyzes existing attempts and cannot request a new attempt"
                );
                crate::runner::analysis_only(&path)?
            } else {
                crate::runner::execute(
                    &result.prepared_manifest,
                    Some(
                        &options
                            .attempt
                            .context("rerun requires --attempt or --analysis-only")?,
                    ),
                    Some(path),
                )?
            }
        }
        _ => bail!("unsupported operation"),
    };
    println!("{}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn exact_modes_and_default_are_enforced() -> Result<()> {
        assert_eq!(parse(vec![])?.mode, Mode::Fast);
        assert_eq!(parse(args(&["-full"]))?.mode, Mode::Full);
        assert_eq!(
            parse(args(&["compare", "-fast", "--prepared", "x"]))?.prepared,
            Some(PathBuf::from("x"))
        );
        assert!(parse(args(&["-fast", "-full"])).is_err());
        assert!(parse(args(&["--full"])).is_err());
        assert!(parse(args(&["--reference"])).is_err());
        Ok(())
    }
    #[test]
    fn executing_harness_accepts_its_recorded_executable_identity() -> Result<()> {
        let frozen = FileIdentity::record(&std::env::current_exe()?)?;
        verify_running_harness(&frozen)?;
        Ok(())
    }
    #[test]
    fn executing_harness_rejects_an_intact_different_prepared_binary() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("frozen-harness");
        std::fs::write(
            &path,
            b"a different previously prepared benchmark executable",
        )?;
        let frozen = FileIdentity::record(&path)?;
        frozen.verify()?;
        let failure = verify_running_harness(&frozen).unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("differs from the prepared harness")
        );
        assert_eq!(
            std::fs::read(&path)?,
            b"a different previously prepared benchmark executable"
        );
        Ok(())
    }
}
