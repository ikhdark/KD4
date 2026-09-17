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
    pub fork_only_on: bool,
    pub explicit_variant_selection: bool,
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
        fork_only_on: true,
        explicit_variant_selection: false,
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
    if result.operation == "scan" {
        result.operation = "run".into();
    }
    ensure!(
        ["run", "prepare", "compare", "import", "rerun", "help"]
            .contains(&result.operation.as_str()),
        "unknown Repo Benchmark operation {}",
        result.operation
    );
    for_token(&mut args, &mut result)?;
    ensure!(
        !result.explicit_variant_selection
            || ["run", "prepare", "compare", "help"].contains(&result.operation.as_str()),
        "variant selection is only valid for run/scan, prepare, or compare; rerun/import preserve the prepared selection"
    );
    Ok(result)
}

fn for_token(args: &mut impl Iterator<Item = String>, result: &mut Options) -> Result<()> {
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fork-only-on" | "--all-variants" | "fork" | "only" => {
                if arg == "fork" {
                    ensure!(
                        args.next().as_deref() == Some("only"),
                        "expected 'fork only on'"
                    );
                }
                if arg == "fork" || arg == "only" {
                    ensure!(args.next().as_deref() == Some("on"), "expected 'only on'");
                }
                let fork_only_on = arg != "--all-variants";
                ensure!(
                    !result.explicit_variant_selection || result.fork_only_on == fork_only_on,
                    "--fork-only-on and --all-variants cannot be combined"
                );
                result.fork_only_on = fork_only_on;
                result.explicit_variant_selection = true;
            }
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
            "Repo Benchmark: one Rust benchmark, scripted then real-model execution.\n\njust repo-benchmark [scan] [-fast|-full] [--fork-only-on|--all-variants]\njust repo-benchmark prepare [-fast|-full] [--fork-only-on|--all-variants] [--reference CHECKOUT] [--fork-ref COMMIT]\njust repo-benchmark compare --prepared MANIFEST [-fast|-full]\njust repo-benchmark import --result RESULT\njust repo-benchmark rerun --result RESULT --attempt ID\njust repo-benchmark rerun --result RESULT --analysis-only\n\nFast: 1 live task. Full: 3 live tasks. Default: fork features on versus upstream, 2 variants.\n--fork-only-on (also: only on / fork only on): explicitly select the default.\n--all-variants: include fork features off, 3 variants.\nDefault reference: newest local stable upstream release tag; verified native builds are reused until the release or build inputs change. No automatic fetch.\nAll execution is sequential and stops when finite work finishes. No performance pass/fail verdict."
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
                ensure!(
                    !options.explicit_variant_selection
                        || options.fork_only_on == prepared.fork_only_on,
                    "variant selection differs from frozen schedule; prepare again"
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
                    fork_only_on: options.fork_only_on,
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
    fn fork_only_on_aliases_select_two_variants_in_both_modes() -> Result<()> {
        assert!(parse(vec![])?.fork_only_on);
        assert!(!parse(vec![])?.explicit_variant_selection);
        for (flag, mode, live_count) in [("-fast", Mode::Fast, 2), ("-full", Mode::Full, 6)] {
            for suffix in [
                vec![],
                vec!["--fork-only-on"],
                vec!["only", "on"],
                vec!["fork", "only", "on"],
            ] {
                let mut values = vec!["scan", flag];
                values.extend(suffix);
                let options = parse(args(&values))?;
                assert_eq!(options.operation, "run");
                assert_eq!(options.mode, mode);
                assert!(options.fork_only_on);
                let scheduled = crate::schedule::schedule(options.mode, options.fork_only_on);
                assert_eq!(scheduled.len(), 84 + live_count);
                assert_eq!(
                    scheduled
                        .iter()
                        .filter(|a| a.segment == crate::schedule::Segment::Scripted)
                        .count(),
                    84
                );
                assert!(
                    scheduled
                        .iter()
                        .all(|a| a.variant != crate::schedule::Variant::ForkOff)
                );
                for variant in [
                    crate::schedule::Variant::ForkOn,
                    crate::schedule::Variant::Reference,
                ] {
                    assert_eq!(
                        scheduled.iter().filter(|a| a.variant == variant).count(),
                        42 + live_count / 2
                    );
                }
            }
        }
        for values in [
            vec!["scan", "only", "off"],
            vec!["scan", "fork", "on"],
            vec!["scan", "only"],
            vec!["rerun", "--fork-only-on"],
            vec!["import", "--fork-only-on"],
            vec!["rerun", "--all-variants"],
            vec!["import", "--all-variants"],
            vec!["scan", "--all-variants", "only", "on"],
            vec!["scan", "--fork-only-on", "--all-variants"],
        ] {
            assert!(parse(args(&values)).is_err(), "{values:?}");
        }
        Ok(())
    }
    #[test]
    fn all_variants_requires_selection_and_saved_operations_inherit() -> Result<()> {
        for (flag, count) in [("-fast", 129), ("-full", 135)] {
            let options = parse(args(&["scan", flag, "--all-variants"]))?;
            assert!(!options.fork_only_on);
            assert!(options.explicit_variant_selection);
            let scheduled = crate::schedule::schedule(options.mode, options.fork_only_on);
            assert_eq!(scheduled.len(), count);
            assert_eq!(
                scheduled
                    .iter()
                    .filter(|a| a.variant == crate::schedule::Variant::ForkOff)
                    .count(),
                count / 3
            );
        }
        for operation in ["run", "prepare", "compare", "rerun", "import"] {
            let options = parse(args(&[operation]))?;
            assert!(options.fork_only_on);
            assert!(!options.explicit_variant_selection);
        }
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
