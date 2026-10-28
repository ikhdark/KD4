use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

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
    run_git_for_stdout_exact_limited(dir, args, env, DEFAULT_GIT_OUTPUT_LIMIT)
}

pub(crate) fn run_git_for_stdout_exact_limited<I, S>(
    dir: &Path,
    args: I,
    env: Option<&[(OsString, OsString)]>,
    stdout_limit: usize,
) -> Result<String, GitToolingError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let run = run_git(crate::git_executable(), dir, collect_git_args(args), env, stdout_limit)?;
    String::from_utf8(run.output.stdout).map_err(|source| GitToolingError::GitOutputUtf8 {
        command: build_command_string(&run.command),
        source,
    })
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
    stdout_limit: usize,
) -> Result<GitRun, GitToolingError> {
    let args_vec = git_args_with_hardening(&args, None);
    let control = GitCommandControl::new();
    let output = run_git_attempt(git, dir, &args_vec, env, stdout_limit, &control)?;
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
        let retry_output = run_git_attempt(git, dir, &retry_args, env, stdout_limit, &control)?;
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
    stdout_limit: usize,
    control: &GitCommandControl,
) -> Result<std::process::Output, GitToolingError> {
    let mut command = Command::new(git);
    command.current_dir(dir);
    if let Some(envs) = env {
        for (key, value) in envs {
            command.env(key, value);
        }
    }
    command.args(args);
    Ok(run_git_output(command, stdout_limit, control)?)
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

pub(crate) const DEFAULT_GIT_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
const GIT_STDERR_LIMIT: usize = 1024 * 1024;

/// One deadline/cancellation owner across all subprocesses of a patch operation.
#[derive(Clone)]
pub(crate) struct GitCommandControl {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl GitCommandControl {
    pub(crate) fn new() -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(30),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn check(&self) -> std::io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted,
                "Git operation cancelled; working files may be partially changed; inspect before retrying"));
        }
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut,
                "Git operation exceeded 30 seconds; working files may be partially changed; inspect before retrying"));
        }
        Ok(())
    }
}

pub(crate) fn check_git_output_limits(
    stdout: &std::fs::File,
    stderr: &std::fs::File,
    stdout_limit: usize,
) -> std::io::Result<()> {
    if stdout.metadata()?.len() > stdout_limit as u64
        || stderr.metadata()?.len() > GIT_STDERR_LIMIT as u64
    {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,
            "Git output exceeds the supported capture limit"));
    }
    Ok(())
}

pub(crate) fn read_git_output(
    mut stdout: std::fs::File,
    mut stderr: std::fs::File,
    status: std::process::ExitStatus,
    stdout_limit: usize,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::io::Seek;
    check_git_output_limits(&stdout, &stderr, stdout_limit)?;
    stdout.rewind()?;
    stderr.rewind()?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    // Recheck actual bytes too: an inherited writer can grow a file after stat.
    stdout.take(stdout_limit as u64 + 1).read_to_end(&mut stdout_bytes)?;
    stderr.take(GIT_STDERR_LIMIT as u64 + 1).read_to_end(&mut stderr_bytes)?;
    if stdout_bytes.len() > stdout_limit || stderr_bytes.len() > GIT_STDERR_LIMIT {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,
            "Git output grew beyond the supported capture limit"));
    }
    Ok(std::process::Output { status, stdout: stdout_bytes, stderr: stderr_bytes })
}

/// Use temporary files, not pipe-reader threads which can outlive cancellation.
/// The existing Windows Job owner contains filters/helpers before they can run.
pub(crate) fn run_git_output(
    mut command: Command,
    stdout_limit: usize,
    control: &GitCommandControl,
) -> std::io::Result<std::process::Output> {
    control.check()?;
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    command.stdin(std::process::Stdio::null())
        .stdout(stdout.try_clone()?).stderr(stderr.try_clone()?);
    #[cfg(windows)]
    let managed = {
        use std::os::windows::process::CommandExt;
        command.creation_flags(codex_utils_pty::WINDOWS_CREATE_SUSPENDED);
        codex_utils_pty::ManagedRootProcess::reserve()?
    };
    let mut child = SyncGitChild {
        child: command.spawn()?,
        #[cfg(windows)]
        managed,
    };
    #[cfg(windows)]
    child.managed.attach_and_resume(child.child.id())?;
    let status = loop {
        control.check()?;
        check_git_output_limits(&stdout, &stderr, stdout_limit)?;
        if let Some(status) = child.child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    read_git_output(stdout, stderr, status, stdout_limit)
}

struct SyncGitChild {
    child: std::process::Child,
    #[cfg(windows)]
    managed: codex_utils_pty::ManagedRootProcess,
}

impl Drop for SyncGitChild {
    fn drop(&mut self) {
        #[cfg(windows)]
        let _ = self.managed.terminate();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            // Bound the cleanup observer too; the Job retains kill-on-close.
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

