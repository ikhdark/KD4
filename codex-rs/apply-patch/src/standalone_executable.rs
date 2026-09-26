use std::ffi::OsString;
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

#[derive(Debug)]
enum PatchInputError {
    EmptyStdin,
    NonUtf8Argument,
    ReadStdin(std::io::Error),
    TooManyArguments,
}

impl PatchInputError {
    fn exit_code(&self) -> i32 {
        match self {
            Self::EmptyStdin | Self::TooManyArguments => 2,
            Self::NonUtf8Argument | Self::ReadStdin(_) => 1,
        }
    }

    fn report(&self) {
        match self {
            Self::EmptyStdin => {
                eprintln!("Usage: apply_patch 'PATCH'\n       echo 'PATCH' | apply_patch");
            }
            Self::NonUtf8Argument => {
                eprintln!("Error: apply_patch requires a UTF-8 PATCH argument.");
            }
            Self::ReadStdin(err) => {
                eprintln!("Error: Failed to read PATCH from stdin.\n{err}");
            }
            Self::TooManyArguments => {
                eprintln!("Error: apply_patch accepts exactly one argument.");
            }
        }
    }
}

/// Expect either one argument (the full apply_patch payload) or read it from stdin.
fn patch_from_args_or_reader(
    args: impl IntoIterator<Item = OsString>,
    reader: impl Read,
) -> Result<String, PatchInputError> {
    let mut args = args.into_iter();
    let patch = match args.next() {
        Some(arg) => arg
            .into_string()
            .map_err(|_| PatchInputError::NonUtf8Argument)?,
        None => {
            let patch = read_patch_input(reader).map_err(PatchInputError::ReadStdin)?;
            if patch.is_empty() {
                return Err(PatchInputError::EmptyStdin);
            }
            patch
        }
    };
    // Refuse extra args to avoid ambiguity.
    if args.next().is_some() {
        return Err(PatchInputError::TooManyArguments);
    }
    Ok(patch)
}

pub fn main() -> ! {
    let exit_code = run_main();
    std::process::exit(exit_code);
}

/// We would prefer to return `std::process::ExitCode`, but its `exit_process()`
/// method is still a nightly API and we want main() to return !.
pub fn run_main() -> i32 {
    run_main_with_args(std::env::args_os().skip(1))
}

/// Runs the `apply_patch` command surface for the arguments after the command
/// name, including the executable's hidden self-invocation flag.
pub fn run_main_with_args(args: impl IntoIterator<Item = OsString>) -> i32 {
    match patch_from_args_or_reader(args, std::io::stdin().lock()) {
        Ok(patch) => run_apply_patch(&patch),
        Err(err) => {
            err.report();
            err.exit_code()
        }
    }
}

/// Execute a patch for standalone and hidden CLI invocations, including recovery output.
fn run_apply_patch(patch: &str) -> i32 {
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
    use super::PatchInputError;
    use super::patch_from_args_or_reader;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;

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

    #[test]
    fn patch_arg_uses_single_utf8_argument() {
        let patch = patch_from_args_or_reader(
            [OsString::from("*** Begin Patch\n*** End Patch")],
            "ignored".as_bytes(),
        )
        .expect("single UTF-8 patch argument should be accepted");

        assert_eq!(patch, "*** Begin Patch\n*** End Patch");
    }

    #[test]
    fn patch_arg_reads_stdin_when_argument_missing() {
        let patch = patch_from_args_or_reader(
            [],
            "*** Begin Patch\n*** Add File: file.txt\n+hello\n*** End Patch".as_bytes(),
        )
        .expect("stdin patch should be accepted when no argument is passed");

        assert_eq!(
            patch,
            "*** Begin Patch\n*** Add File: file.txt\n+hello\n*** End Patch"
        );
    }

    #[test]
    fn patch_arg_rejects_empty_stdin() {
        let err = patch_from_args_or_reader([], "".as_bytes())
            .expect_err("empty stdin should be rejected");

        assert!(matches!(err, PatchInputError::EmptyStdin));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn patch_arg_rejects_invalid_utf8_stdin() {
        let err = patch_from_args_or_reader([], std::io::Cursor::new(vec![0xff]))
            .expect_err("invalid UTF-8 stdin should be rejected");

        assert!(matches!(
            &err,
            PatchInputError::ReadStdin(source) if source.kind() == std::io::ErrorKind::InvalidData
        ));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn patch_arg_rejects_non_utf8_argument() {
        use std::os::windows::ffi::OsStringExt;

        let err = patch_from_args_or_reader([OsString::from_wide(&[0xD800])], "ignored".as_bytes())
            .expect_err("non-UTF-8 patch argument should be rejected");

        assert!(matches!(err, PatchInputError::NonUtf8Argument));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn patch_arg_rejects_extra_arguments() {
        let err = patch_from_args_or_reader(
            [OsString::from("patch"), OsString::from("extra")],
            "ignored".as_bytes(),
        )
        .expect_err("extra patch arguments should be rejected");

        assert!(matches!(err, PatchInputError::TooManyArguments));
        assert_eq!(err.exit_code(), 2);
    }
}
