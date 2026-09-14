use std::io::Read;
use std::io::Write;

pub fn main() -> ! {
    let exit_code = run_main();
    std::process::exit(exit_code);
}

/// We would prefer to return `std::process::ExitCode`, but its `exit_process()`
/// method is still a nightly API and we want main() to return !.
pub fn run_main() -> i32 {
    // Expect either one argument (the full apply_patch payload) or read it from stdin.
    let mut args = std::env::args_os();
    let _argv0 = args.next();

    let patch_arg = match args.next() {
        Some(arg) => match arg.into_string() {
            Ok(s) => s,
            Err(_) => {
                eprintln!("Error: apply_patch requires a UTF-8 PATCH argument.");
                return 1;
            }
        },
        None => {
            // No argument provided; attempt to read the patch from stdin.
            let mut buf = String::new();
            match std::io::stdin().read_to_string(&mut buf) {
                Ok(_) => {
                    if buf.is_empty() {
                        eprintln!("Usage: apply_patch 'PATCH'\n       echo 'PATCH' | apply_patch");
                        return 2;
                    }
                    buf
                }
                Err(err) => {
                    eprintln!("Error: Failed to read PATCH from stdin.\n{err}");
                    return 1;
                }
            }
        }
    };

    // Refuse extra args to avoid ambiguity.
    if args.next().is_some() {
        eprintln!("Error: apply_patch accepts exactly one argument.");
        return 2;
    }

    run_apply_patch(&patch_arg)
}

/// Execute a patch for standalone and hidden CLI invocations, including recovery output.
pub fn run_apply_patch(patch: &str) -> i32 {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let cwd = match codex_utils_absolute_path::AbsolutePathBuf::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            eprintln!("Error: Failed to determine current directory.\n{err}");
            return 1;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("Error: Failed to initialize runtime.\n{err}");
            return 1;
        }
    };
    // TODO(anp): Discover the standalone executable cwd as PathUri directly.
    let cwd = codex_utils_path_uri::PathUri::from_abs_path(&cwd);
    match runtime.block_on(crate::apply_patch(
        patch,
        &cwd,
        &mut stdout,
        &mut stderr,
        codex_exec_server::LOCAL_FS.as_ref(),
        /*sandbox*/ None,
    )) {
        Ok(_) => {
            // Flush to ensure output ordering when used in pipelines.
            match stdout.flush() {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("Error: Patch applied, but failed to flush its output: {err}");
                    1
                }
            }
        }
        Err(failure) => {
            let _ = print_failure_delta(failure.delta(), &mut stderr);
            1
        }
    }
}

fn print_failure_delta(
    delta: &crate::AppliedPatchDelta,
    stderr: &mut impl Write,
) -> std::io::Result<()> {
    if !delta.is_empty() {
        writeln!(
            stderr,
            "Changes committed before the failure (do not retry the whole patch):"
        )?;
        for change in delta.changes() {
            match &change.change {
                crate::AppliedPatchFileChange::Add { .. } => {
                    writeln!(stderr, "A {}", change.path.display())?;
                }
                crate::AppliedPatchFileChange::Delete { .. } => {
                    writeln!(stderr, "D {}", change.path.display())?;
                }
                crate::AppliedPatchFileChange::Update { move_path, .. } => {
                    if let Some(destination) = move_path {
                        writeln!(
                            stderr,
                            "M {} -> {}",
                            change.path.display(),
                            destination.display()
                        )?;
                    } else {
                        writeln!(stderr, "M {}", change.path.display())?;
                    }
                }
            }
        }
    }
    if !delta.is_exact() {
        writeln!(
            stderr,
            "The change list is incomplete; additional partial filesystem effects may exist."
        )?;
    }
    Ok(())
}
