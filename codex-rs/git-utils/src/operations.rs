use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::DISABLED_HOOKS_PATH;
use crate::GitToolingError;
use crate::info::get_git_repo_root;
use crate::info::is_dubious_ownership_stderr;

pub(crate) fn ensure_git_repository(path: &Path) -> Result<(), GitToolingError> {
    match run_git_for_stdout(
        path,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--is-inside-work-tree"),
        ],
        /*env*/ None,
    ) {
        Ok(output) if output.trim() == "true" => Ok(()),
        Ok(_) => Err(GitToolingError::NotAGitRepository {
            path: path.to_path_buf(),
        }),
        Err(GitToolingError::GitCommand { status, .. }) if status.code() == Some(128) => {
            Err(GitToolingError::NotAGitRepository {
                path: path.to_path_buf(),
            })
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn resolve_head(path: &Path) -> Result<Option<String>, GitToolingError> {
    match run_git_for_stdout(
        path,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("HEAD"),
        ],
        /*env*/ None,
    ) {
        Ok(sha) => Ok(Some(sha)),
        Err(GitToolingError::GitCommand { status, .. }) if status.code() == Some(128) => Ok(None),
        Err(other) => Err(other),
    }
}

pub(crate) fn resolve_repository_root(path: &Path) -> Result<PathBuf, GitToolingError> {
    let root = run_git_for_stdout(
        path,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ],
        /*env*/ None,
    )?;
    Ok(PathBuf::from(root))
}

pub(crate) fn run_git_for_stdout<I, S>(
    dir: &Path,
    args: I,
    env: Option<&[(OsString, OsString)]>,
) -> Result<String, GitToolingError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_for_stdout_exact(dir, args, env).map(|value| value.trim().to_string())
}

/// Source bytes and NUL-delimited path inventories must not be whitespace-trimmed.
pub(crate) fn run_git_for_stdout_exact<I, S>(
    dir: &Path,
    args: I,
    env: Option<&[(OsString, OsString)]>,
) -> Result<String, GitToolingError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let run = run_git_for_stdout_from(crate::git_executable(), dir, args, env)?;
    String::from_utf8(run.output.stdout).map_err(|source| GitToolingError::GitOutputUtf8 {
        command: build_command_string(&run.command),
        source,
    })
}

fn run_git_for_stdout_from<I, S>(
    git: &Path,
    dir: &Path,
    args: I,
    env: Option<&[(OsString, OsString)]>,
) -> Result<GitRun, GitToolingError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git(git, dir, collect_git_args(args), env)
}

fn collect_git_args<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .map(|arg| OsString::from(arg.as_ref()))
        .collect()
}

fn run_git(
    git: &Path,
    dir: &Path,
    args: Vec<OsString>,
    env: Option<&[(OsString, OsString)]>,
) -> Result<GitRun, GitToolingError> {
    let args_vec = git_args_with_hardening(&args, None);
    let output = run_git_attempt(git, dir, &args_vec, env)?;
    if output.status.success() {
        return Ok(GitRun {
            command: args_vec,
            output,
        });
    }

    let mut command_args = args_vec;
    let mut status = output.status;
    let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if is_dubious_ownership_stderr(status.success(), stderr.as_bytes())
        && let Some(repo_root) = get_git_repo_root(dir)
    {
        let retry_args = git_args_with_hardening(&args, Some(repo_root.as_path()));
        let retry_output = run_git_attempt(git, dir, &retry_args, env)?;
        if retry_output.status.success() {
            return Ok(GitRun {
                command: retry_args,
                output: retry_output,
            });
        }
        command_args = retry_args;
        status = retry_output.status;
        stderr = String::from_utf8_lossy(&retry_output.stderr)
            .trim()
            .to_string();
    }

    Err(GitToolingError::GitCommand {
        command: build_command_string(&command_args),
        status,
        stderr,
    })
}

fn git_args_with_hardening(args: &[OsString], safe_directory: Option<&Path>) -> Vec<OsString> {
    let mut args_vec = Vec::with_capacity(args.len() + 4);
    // Keep internal Git helper commands independent of configured hook directories.
    args_vec.push(OsString::from("-c"));
    args_vec.push(OsString::from(format!(
        "core.hooksPath={DISABLED_HOOKS_PATH}"
    )));
    if let Some(safe_directory) = safe_directory {
        args_vec.push(OsString::from("-c"));
        args_vec.push(OsString::from(format!(
            "safe.directory={}",
            safe_directory.to_string_lossy()
        )));
    }
    args_vec.extend(args.iter().cloned());
    args_vec
}

fn run_git_attempt(
    git: &Path,
    dir: &Path,
    args: &[OsString],
    env: Option<&[(OsString, OsString)]>,
) -> Result<std::process::Output, GitToolingError> {
    let mut command = Command::new(git);
    command.current_dir(dir);
    if let Some(envs) = env {
        for (key, value) in envs {
            command.env(key, value);
        }
    }
    command.args(args);
    Ok(command.output()?)
}

fn build_command_string(args: &[OsString]) -> String {
    if args.is_empty() {
        return "git".to_string();
    }
    let joined = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    format!("git {joined}")
}

struct GitRun {
    command: Vec<OsString>,
    output: std::process::Output,
}

