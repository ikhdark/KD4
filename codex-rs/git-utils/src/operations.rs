use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::GitToolingError;
use crate::info::get_git_repo_root;

const DISABLED_HOOKS_PATH: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

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

pub(crate) fn run_git_for_status<I, S>(
    dir: &Path,
    args: I,
    env: Option<&[(OsString, OsString)]>,
) -> Result<(), GitToolingError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_for_status_from(Path::new("git"), dir, args, env)?;
    Ok(())
}

fn run_git_for_status_from<I, S>(
    git: &Path,
    dir: &Path,
    args: I,
    env: Option<&[(OsString, OsString)]>,
) -> Result<(), GitToolingError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git(git, dir, collect_git_args(args), env, false)?;
    Ok(())
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
    let run = run_git_for_stdout_from(Path::new("git"), dir, args, env)?;
    String::from_utf8(run.output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|source| GitToolingError::GitOutputUtf8 {
            command: run.command,
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
    run_git(git, dir, collect_git_args(args), env, true)
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
    allow_safe_directory_retry: bool,
) -> Result<GitRun, GitToolingError> {
    let args_vec = git_args_with_hardening(&args, None);
    let command_string = build_command_string(&args_vec);
    let output = run_git_attempt(git, dir, &args_vec, env)?;
    if output.status.success() {
        return Ok(GitRun {
            command: command_string,
            output,
        });
    }

    let mut command_string = command_string;
    let mut status = output.status;
    let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if allow_safe_directory_retry
        && is_dubious_ownership_stderr(status.success(), stderr.as_bytes())
        && let Some(repo_root) = get_git_repo_root(dir)
    {
        let retry_args = git_args_with_hardening(&args, Some(repo_root.as_path()));
        let retry_command_string = build_command_string(&retry_args);
        let retry_output = run_git_attempt(git, dir, &retry_args, env)?;
        if retry_output.status.success() {
            return Ok(GitRun {
                command: retry_command_string,
                output: retry_output,
            });
        }
        command_string = retry_command_string;
        status = retry_output.status;
        stderr = String::from_utf8_lossy(&retry_output.stderr)
            .trim()
            .to_string();
    }

    Err(GitToolingError::GitCommand {
        command: command_string,
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

fn is_dubious_ownership_stderr(status_success: bool, stderr: &[u8]) -> bool {
    if status_success {
        return false;
    }
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    stderr.contains("detected dubious ownership") && stderr.contains("safe.directory")
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
    command: String,
    output: std::process::Output,
}

#[cfg(test)]
mod tests {
    use super::run_git_for_status_from;
    use super::run_git_for_stdout_from;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::path::Path;

    #[test]
    fn sync_git_inspection_retries_dubious_ownership_with_safe_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let nested = repo.join("nested");
        fs::create_dir_all(repo.join(".git")).expect("create git marker");
        fs::create_dir_all(&nested).expect("create nested dir");
        let log = temp.path().join("git.log");
        let git = write_fake_git(temp.path(), fake_git_dubious_then_success(&log));

        let run = run_git_for_stdout_from(&git, &nested, ["rev-parse", "HEAD"], None)
            .expect("dubious ownership should retry with safe.directory");
        let output = String::from_utf8(run.output.stdout).expect("fake Git stdout is UTF-8");

        assert_eq!(output.trim(), "retried-ok");
        let log = fs::read_to_string(log).expect("read fake git log");
        let entries = log.lines().collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].contains("core.hooksPath="));
        assert!(!entries[0].contains("safe.directory="));
        assert!(entries[1].contains("core.hooksPath="));
        assert!(entries[1].contains(&format!("safe.directory={}", repo.to_string_lossy())));
    }

    #[test]
    fn sync_git_inspection_does_not_retry_unrelated_failures() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).expect("create git marker");
        let log = temp.path().join("git.log");
        let git = write_fake_git(temp.path(), fake_git_other_failure(&log));

        let err = match run_git_for_stdout_from(&git, &repo, ["rev-parse", "HEAD"], None) {
            Ok(_) => panic!("unrelated failure should not retry"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("not a git repository"));
        let log = fs::read_to_string(log).expect("read fake git log");
        assert_eq!(log.lines().count(), 1);
    }

    #[test]
    fn sync_git_status_does_not_retry_dubious_ownership() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).expect("create git marker");
        let log = temp.path().join("git.log");
        let git = write_fake_git(temp.path(), fake_git_dubious_then_success(&log));

        run_git_for_status_from(&git, &repo, ["read-tree", "--reset", "HEAD"], None)
            .expect_err("status-only mutating commands must not retry with safe.directory");

        let log = fs::read_to_string(log).expect("read fake git log");
        let entries = log.lines().collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].contains("safe.directory="));
    }

    fn write_fake_git(dir: &Path, script: String) -> std::path::PathBuf {
        let git = dir.join(if cfg!(windows) { "git.cmd" } else { "git" });
        fs::write(&git, script).expect("write fake git");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&git).expect("fake git metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&git, permissions).expect("mark fake git executable");
        }
        git
    }

    #[cfg(windows)]
    fn fake_git_dubious_then_success(log: &Path) -> String {
        format!(
            "@echo off\r\necho %*>>\"{}\"\r\necho %* | findstr /C:\"safe.directory=\" >NUL\r\nif errorlevel 1 (\r\n  echo fatal: detected dubious ownership in repository at '%CD%' 1>&2\r\n  echo To add an exception for this directory, call: 1>&2\r\n  echo     git config --global --add safe.directory %CD% 1>&2\r\n  exit /b 128\r\n)\r\necho retried-ok\r\nexit /b 0\r\n",
            log.display()
        )
    }

    #[cfg(unix)]
    fn fake_git_dubious_then_success(log: &Path) -> String {
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \" $* \" in\n  *safe.directory=*) printf 'retried-ok\\n'; exit 0 ;;\nesac\nprintf '%s\\n' \"fatal: detected dubious ownership in repository at '$PWD'\" >&2\nprintf '%s\\n' \"To add an exception for this directory, call:\" >&2\nprintf '%s\\n' \"git config --global --add safe.directory $PWD\" >&2\nexit 128\n",
            log.display()
        )
    }

    #[cfg(windows)]
    fn fake_git_other_failure(log: &Path) -> String {
        format!(
            "@echo off\r\necho %*>>\"{}\"\r\necho fatal: not a git repository 1>&2\r\nexit /b 128\r\n",
            log.display()
        )
    }

    #[cfg(unix)]
    fn fake_git_other_failure(log: &Path) -> String {
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' 'fatal: not a git repository' >&2\nexit 128\n",
            log.display()
        )
    }
}
