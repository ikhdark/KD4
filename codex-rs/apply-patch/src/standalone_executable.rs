use std::io::Read;
use std::io::Write;

use crate::parser::MAX_PATCH_INPUT_BYTES;

fn read_patch_input(reader: impl Read) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_PATCH_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PATCH_INPUT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("PATCH input exceeds the {MAX_PATCH_INPUT_BYTES}-byte limit"),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

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
            match read_patch_input(std::io::stdin().lock()) {
                Ok(buf) => {
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
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn read_patch_input_preserves_input_and_rejects_oversize_without_draining() {
        use std::io::Read as _;

        assert_eq!(
            super::read_patch_input(b"normal prompt\n".as_slice()).unwrap(),
            "normal prompt\n"
        );
        let mut input = std::io::repeat(b'x').take(super::MAX_PATCH_INPUT_BYTES as u64 + 2);
        let error = super::read_patch_input(&mut input).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds"));
        assert_eq!(input.limit(), 1, "must stop after the first excess byte");
    }
}
