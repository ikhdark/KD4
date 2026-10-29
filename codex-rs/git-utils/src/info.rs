use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use codex_file_system::ExecutorFileSystem;
use codex_file_system::FindUpErrorPolicy;
use codex_file_system::find_nearest_native_ancestor_with_markers;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;
use futures::stream;
use serde::Deserialize;
use serde::Serialize;
use tokio::process::Command;
use tokio::time::Duration as TokioDuration;
use tokio::time::timeout;

use crate::DISABLED_HOOKS_PATH;
use crate::GitSha;
pub use codex_protocol::protocol::GitInfo;

/// Return `true` if the project folder specified by the `Config` is inside a
/// Git repository.
///
/// The check walks up the directory hierarchy looking for a `.git` file or
/// directory (note `.git` can be a file that contains a `gitdir` entry). This
/// approach does **not** require the `git` binary or the `git2` crate and is
/// therefore fairly lightweight.
///
/// Linked worktrees are detected through their `.git` file and return the
/// checkout root, not the main repository's root.
pub fn get_git_repo_root(base_dir: &Path) -> Option<PathBuf> {
    let base = if base_dir.is_dir() {
        base_dir
    } else {
        base_dir.parent()?
    };
    find_ancestor_git_entry(base).map(|(repo_root, _)| repo_root)
}

/// Return the repository root for `cwd` using the provided filesystem.
///
/// This mirrors [`get_git_repo_root`] for local paths, but works when `cwd`
/// only exists inside a selected remote environment.
pub async fn get_git_repo_root_with_fs(
    fs: &dyn ExecutorFileSystem,
    cwd: &AbsolutePathBuf,
) -> Option<AbsolutePathBuf> {
    let cwd_uri = PathUri::from_abs_path(cwd);
    let base = match fs.get_metadata(&cwd_uri, /*sandbox*/ None).await {
        Ok(metadata) if metadata.is_directory => cwd.clone(),
        _ => cwd.parent()?,
    };
    find_nearest_native_ancestor_with_markers(
        fs,
        &base,
        vec![".git".to_string()],
        FindUpErrorPolicy::Ignore,
        /*sandbox*/ None,
    )
    .await
    .ok()?
}

/// Timeout for git commands to prevent freezing on large repositories
const GIT_COMMAND_TIMEOUT: TokioDuration = TokioDuration::from_secs(5);
const UNTRACKED_DIFF_CONCURRENCY: usize = 8;
const MAX_DIFF_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Confirmed, runtime-only repository identity plus stable Git metadata.
/// Mutable worktree, index, and untracked-file observations stay external.
#[derive(Clone, Debug)]
pub struct RepositoryContext {
    pub repo_root: PathBuf,
    pub common_dir: PathBuf,
    pub worktree_git_dir: PathBuf,
    pub git_info: GitInfo,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GitDiffToRemote {
    pub sha: GitSha,
    pub diff: String,
}

/// Collect git repository information from the given working directory using command-line git.
/// Returns None if no git repository is found or if git operations fail.
/// Uses timeouts to prevent freezing on large repositories.
/// All git commands (except the initial repo check) run in parallel for better performance.
pub async fn collect_git_info(cwd: &Path) -> Option<GitInfo> {
    // Check if we're in a git repository first
    let is_git_repo = run_git_command_with_timeout(&["rev-parse", "--git-dir"], cwd)
        .await?
        .status
        .success();

    if !is_git_repo {
        return None;
    }

    Some(collect_git_info_assume_git_repo(cwd).await)
}

/// Discover a small repository context for callers that naturally own it.
/// Standalone callers should continue to use [`collect_git_info`].
pub async fn discover_repository_context(cwd: &Path) -> Option<RepositoryContext> {
    let output = run_git_command_with_timeout(
        &[
            "rev-parse",
            "--show-toplevel",
            "--git-common-dir",
            "--git-dir",
        ],
        cwd,
    )
    .await?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let native_cwd = cwd.to_path_buf();
    let (repo_root, common_dir, worktree_git_dir) = tokio::task::spawn_blocking(move || {
        let mut lines = stdout.lines();
        let repo_root = resolve_git_identity_path(&native_cwd, lines.next()?)?;
        let common_dir = resolve_git_identity_path(&native_cwd, lines.next()?)?;
        let worktree_git_dir = resolve_git_identity_path(&native_cwd, lines.next()?)?;
        if lines.next().is_some() {
            return None;
        }
        Some((repo_root, common_dir, worktree_git_dir))
    })
    .await
    .ok()??;

    Some(RepositoryContext {
        repo_root,
        common_dir,
        worktree_git_dir,
        git_info: collect_git_info_assume_git_repo(cwd).await,
    })
}

fn resolve_git_identity_path(cwd: &Path, output: &str) -> Option<PathBuf> {
    let path = PathBuf::from(output.trim());
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    path.canonicalize().ok()
}

/// Collect Git metadata when repository membership has already been established.
pub async fn collect_git_info_assume_git_repo(cwd: &Path) -> GitInfo {
    let (head_result, url_result) = tokio::join!(
        run_git_command_with_timeout(&["rev-parse", "HEAD", "--abbrev-ref", "HEAD"], cwd),
        run_git_command_with_timeout(&["remote", "get-url", "origin"], cwd)
    );

    let mut git_info = GitInfo {
        commit_hash: None,
        branch: None,
        repository_url: None,
    };

    // `rev-parse` prints one line per argument, preserving the previous
    // detached-HEAD marker while avoiding a second process.
    if let Some(output) = head_result
        && output.status.success()
        && let Ok(stdout) = String::from_utf8(output.stdout)
    {
        let mut lines = stdout.lines();
        if let (Some(hash), Some(branch)) = (lines.next(), lines.next()) {
            git_info.commit_hash = Some(GitSha::new(hash.trim()));
            let branch = branch.trim();
            if branch != "HEAD" {
                git_info.branch = Some(branch.to_string());
            }
        }
    }

    // Process repository URL
    if let Some(output) = url_result
        && output.status.success()
        && let Ok(url) = String::from_utf8(output.stdout)
    {
        git_info.repository_url = Some(url.trim().to_string());
    }

    git_info
}

/// Collect fetch remotes in a multi-root-friendly format: {"origin": "https://..."},
/// without checking whether `cwd` is in a git repo.
pub async fn get_git_remote_urls_assume_git_repo(cwd: &Path) -> Option<BTreeMap<String, String>> {
    get_git_remote_urls_result(cwd).await.ok().flatten()
}

/// Distinguishes a successful empty remote list from an unavailable observation.
pub async fn get_git_remote_urls_result(
    cwd: &Path,
) -> std::io::Result<Option<BTreeMap<String, String>>> {
    let output = run_git_command_with_timeout(&["remote", "-v"], cwd)
        .await
        .ok_or_else(|| std::io::Error::other("Git remote query failed or timed out"))?;
    if !output.status.success() {
        return Err(std::io::Error::other("Git remote query failed"));
    }
    let stdout = String::from_utf8(output.stdout).map_err(std::io::Error::other)?;
    let remotes = parse_git_remote_urls(stdout.as_str());
    if remotes.is_none() && !stdout.trim().is_empty() {
        return Err(std::io::Error::other("invalid Git remote list"));
    }
    Ok(remotes)
}

/// Return the current HEAD commit hash without checking whether `cwd` is in a git repo.
pub async fn get_head_commit_hash(cwd: &Path) -> Option<GitSha> {
    get_head_commit_hash_result(cwd).await.ok().flatten()
}

/// Distinguishes an unborn HEAD from command, decoding, and timeout failures.
pub async fn get_head_commit_hash_result(cwd: &Path) -> std::io::Result<Option<GitSha>> {
    let output = run_git_command_with_timeout(&["rev-parse", "--verify", "--quiet", "HEAD"], cwd)
        .await
        .ok_or_else(|| std::io::Error::other("Git HEAD query failed or timed out"))?;
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
            // A quiet lookup alone cannot prove absence. Confirm that HEAD
            // names a branch whose ref really does not exist.
            if let Some(symbolic) =
                run_git_command_with_timeout(&["symbolic-ref", "--quiet", "HEAD"], cwd).await
                && symbolic.status.success()
                && let Ok(reference) = std::str::from_utf8(&symbolic.stdout)
                && reference.trim().starts_with("refs/heads/")
                && let Some(missing) = run_git_command_with_timeout(
                    &["show-ref", "--verify", "--quiet", reference.trim()],
                    cwd,
                )
                .await
                && missing.status.code() == Some(1)
                && missing.stderr.is_empty()
            {
                return Ok(None);
            }
        }
        return Err(std::io::Error::other("Git HEAD query failed"));
    }
    let stdout = String::from_utf8(output.stdout).map_err(std::io::Error::other)?;
    let hash = stdout.trim();
    if !matches!(hash.len(), 40 | 64) || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(std::io::Error::other("invalid Git HEAD hash"));
    }
    Ok(Some(GitSha::new(hash)))
}

pub fn canonicalize_git_remote_url(url: &str) -> Option<String> {
    let url = trim_git_suffix(url.trim().trim_end_matches('/'));
    if url.is_empty() {
        return None;
    }

    if let Some((scheme, rest)) = url.split_once("://") {
        return canonicalize_git_url_like_remote(scheme, rest);
    }

    if let Some((host_part, path)) = parse_scp_like_remote(url) {
        return canonicalize_git_remote_host_path(host_part, path, /*default_port*/ None);
    }

    let (host_part, path) = url.split_once('/')?;
    canonicalize_git_remote_host_path(host_part, path, /*default_port*/ None)
}

fn canonicalize_git_url_like_remote(scheme: &str, rest: &str) -> Option<String> {
    let default_port = match scheme {
        "git" => Some("9418"),
        "http" => Some("80"),
        "https" => Some("443"),
        "ssh" => Some("22"),
        _ => return None,
    };

    let rest = rest
        .find(['?', '#'])
        .map_or(rest, |suffix_index| &rest[..suffix_index]);
    let (host_part, path) = rest.split_once('/')?;
    canonicalize_git_remote_host_path(host_part, path, default_port)
}

fn parse_scp_like_remote(remote: &str) -> Option<(&str, &str)> {
    if remote.contains('/')
        && remote
            .find('/')
            .is_some_and(|slash| remote.find(':').is_none_or(|colon| slash < colon))
    {
        return None;
    }

    let (host_part, path) = remote.split_once(':')?;
    if host_part.is_empty() || path.is_empty() {
        return None;
    }
    Some((host_part, path))
}

fn canonicalize_git_remote_host_path(
    host_part: &str,
    path: &str,
    default_port: Option<&str>,
) -> Option<String> {
    let host = normalize_remote_host(
        host_part
            .rsplit_once('@')
            .map_or(host_part, |(_, host)| host)
            .trim()
            .trim_end_matches('/'),
        default_port,
    );
    if host.is_empty() {
        return None;
    }

    let path = trim_git_suffix(path.trim().trim_matches('/'));
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let [owner, repo, ..] = components.as_slice() else {
        return None;
    };
    if matches!((*owner, *repo), ("." | "..", _) | (_, "." | "..")) {
        return None;
    }
    let path = components.join("/");

    if host == "github.com" {
        Some(format!("{host}/{}", path.to_ascii_lowercase()))
    } else {
        Some(format!("{host}/{path}"))
    }
}

fn normalize_remote_host(host: &str, default_port: Option<&str>) -> String {
    let host = host.to_ascii_lowercase();
    if let Some(default_port) = default_port
        && let Some((host_without_port, port)) = host.rsplit_once(':')
        && port == default_port
    {
        return host_without_port.to_string();
    }
    host
}

fn trim_git_suffix(value: &str) -> &str {
    value.strip_suffix(".git").unwrap_or(value)
}

pub async fn get_has_changes(cwd: &Path) -> Option<bool> {
    let git = crate::git_executable_async().await;
    let fsmonitor = detect_local_fsmonitor_override(git, cwd).await;
    let output =
        run_git_command_with_timeout_from(git, &["status", "--porcelain"], cwd, fsmonitor).await?;
    if !output.status.success() {
        return None;
    }

    Some(!output.stdout.is_empty())
}

fn parse_git_remote_urls(stdout: &str) -> Option<BTreeMap<String, String>> {
    let mut remotes = BTreeMap::new();
    for line in stdout.lines() {
        let Some(fetch_line) = line.strip_suffix(" (fetch)") else {
            continue;
        };

        let Some((name, url_part)) = fetch_line
            .split_once('\t')
            .or_else(|| fetch_line.split_once(' '))
        else {
            continue;
        };

        let url = url_part.trim_start();
        if !url.is_empty() {
            remotes.insert(name.to_string(), url.to_string());
        }
    }

    if remotes.is_empty() {
        None
    } else {
        Some(remotes)
    }
}

/// A minimal commit summary entry used for pickers (subject + timestamp + sha).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitLogEntry {
    pub sha: String,
    /// Unix timestamp (seconds since epoch) of the commit time (committer time).
    pub timestamp: i64,
    /// Single-line subject of the commit message.
    pub subject: String,
}

/// Return the last `limit` commits reachable from HEAD for the current branch.
/// Each entry contains the SHA, commit timestamp (seconds), and subject line.
/// Returns an empty vector if not in a git repo or on error/timeout.
pub async fn recent_commits(cwd: &Path, limit: usize) -> Vec<CommitLogEntry> {
    let fmt = "%H%x1f%ct%x1f%s"; // <sha> <US> <commit_time> <US> <subject>
    let limit_arg = (limit > 0).then(|| limit.to_string());
    let mut args: Vec<String> = vec!["log".to_string()];
    if let Some(n) = &limit_arg {
        args.push("-n".to_string());
        args.push(n.clone());
    }
    args.push(format!("--pretty=format:{fmt}"));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // The captured log result already supplies the quiet repository/error
    // discriminator, without a separate membership subprocess.
    let Some(log_out) = run_git_command_with_timeout(&arg_refs, cwd).await else {
        return Vec::new();
    };
    if !log_out.status.success() {
        return Vec::new();
    }

    let text = String::from_utf8_lossy(&log_out.stdout);
    let mut entries: Vec<CommitLogEntry> = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\u{001f}');
        let sha = parts.next().unwrap_or("").trim();
        let ts_s = parts.next().unwrap_or("").trim();
        let subject = parts.next().unwrap_or("").trim();
        if sha.is_empty() || ts_s.is_empty() {
            continue;
        }
        let timestamp = ts_s.parse::<i64>().unwrap_or(0);
        entries.push(CommitLogEntry {
            sha: sha.to_string(),
            timestamp,
            subject: subject.to_string(),
        });
    }

    entries
}

/// Returns the closest git sha to HEAD that is on a remote as well as the diff to that sha.
pub async fn git_diff_to_remote(cwd: &Path) -> Option<GitDiffToRemote> {
    // Tracked diffs cover the whole repository with root-relative paths, while untracked
    // listings cover only the command directory with paths relative to it. Run from the root.
    let root = get_git_repo_root(cwd)?;

    let remotes = get_git_remotes(&root).await?;
    let branches = branch_ancestry(&root, &remotes).await?;
    let base_sha = find_closest_sha(&root, &branches, &remotes).await?;
    let diff = diff_against_sha(&root, &base_sha).await?;

    Some(GitDiffToRemote {
        sha: base_sha,
        diff,
    })
}

/// Run a git command with a timeout to prevent blocking on large repositories
async fn run_git_command_with_timeout(args: &[&str], cwd: &Path) -> Option<std::process::Output> {
    // These callers only inspect repository metadata. Worktree workflows probe
    // once and pass their override directly to the lower-level runner.
    run_git_command_with_timeout_from(
        crate::git_executable_async().await,
        args,
        cwd,
        crate::FsmonitorOverride::Disabled,
    )
    .await
}

struct LocalFsmonitorProbeRunner<'a> {
    git: &'a Path,
    cwd: &'a Path,
}

impl crate::FsmonitorProbeRunner for LocalFsmonitorProbeRunner<'_> {
    async fn run_probe(&mut self, args: &[&str]) -> Option<Vec<u8>> {
        // Both probes are fast, bounded metadata queries that do not inspect the
        // worktree or index, so do not reduce the requested command's timeout.
        // Keep the raw configuration query intact while containing any helper
        // processes started by the selected Git executable or wrapper.
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        let output = run_git_command_attempt(
            self.git,
            &args,
            self.cwd,
            crate::FsmonitorOverride::Disabled,
            crate::operations::DEFAULT_GIT_OUTPUT_LIMIT,
        )
        .await?;
        output.status.success().then_some(output.stdout)
    }
}

async fn detect_local_fsmonitor_override(git: &Path, cwd: &Path) -> crate::FsmonitorOverride {
    let mut runner = LocalFsmonitorProbeRunner { git, cwd };
    crate::detect_fsmonitor_override(&mut runner).await
}

async fn run_git_command_with_timeout_from(
    git: &Path,
    args: &[&str],
    cwd: &Path,
    fsmonitor: crate::FsmonitorOverride,
) -> Option<std::process::Output> {
    let args = args.iter().map(OsString::from).collect::<Vec<_>>();
    run_git_command_with_timeout_os_from(git, &args, cwd, fsmonitor).await
}

async fn run_git_command_with_timeout_os_from(
    git: &Path,
    args: &[OsString],
    cwd: &Path,
    fsmonitor: crate::FsmonitorOverride,
) -> Option<std::process::Output> {
    run_git_command_with_timeout_os_from_limited(
        git, args, cwd, fsmonitor, crate::operations::DEFAULT_GIT_OUTPUT_LIMIT,
    ).await
}

async fn run_git_command_with_timeout_os_from_limited(
    git: &Path,
    args: &[OsString],
    cwd: &Path,
    fsmonitor: crate::FsmonitorOverride,
    stdout_limit: usize,
) -> Option<std::process::Output> {
    let command_args = git_inspection_args_os(args, fsmonitor, None);
    let output = run_git_command_attempt(git, &command_args, cwd, fsmonitor, stdout_limit).await?;
    let Some(retry_args) = safe_directory_retry_args_os(
        args,
        cwd,
        fsmonitor,
        output.status.success(),
        &output.stderr,
    ) else {
        return Some(output);
    };

    run_git_command_attempt(git, &retry_args, cwd, fsmonitor, stdout_limit).await
}

fn safe_directory_retry_args_os(
    args: &[OsString],
    cwd: &Path,
    fsmonitor: crate::FsmonitorOverride,
    status_success: bool,
    stderr: &[u8],
) -> Option<Vec<OsString>> {
    if !is_passive_git_inspection_os(args) || !is_dubious_ownership_stderr(status_success, stderr) {
        return None;
    }
    let repo_root = get_git_repo_root(cwd)?;
    Some(git_inspection_args_os(
        args,
        fsmonitor,
        Some(repo_root.as_path()),
    ))
}

fn is_passive_git_inspection_os(args: &[OsString]) -> bool {
    // `--literal-pathspecs` only changes how paths match; classify the command after it.
    let args = match args {
        [option, rest @ ..] if option == "--literal-pathspecs" => rest,
        _ => args,
    };
    let Some(command) = args.first().and_then(|arg| arg.to_str()) else {
        return false;
    };
    if command == "remote" {
        return matches!(args, [_])
            || matches!(args, [_, option] if option == "-v")
            || matches!(args, [_, subcommand, _] if subcommand == "get-url");
    }
    matches!(
        command,
        "branch"
            | "diff"
            | "for-each-ref"
            | "log"
            | "ls-files"
            | "merge-base"
            | "rev-list"
            | "rev-parse"
            | "status"
            | "symbolic-ref"
    )
}

fn git_inspection_args_os(
    args: &[OsString],
    fsmonitor: crate::FsmonitorOverride,
    safe_directory: Option<&Path>,
) -> Vec<OsString> {
    let mut command_args = vec![
        OsString::from("-c"),
        OsString::from(format!("core.hooksPath={DISABLED_HOOKS_PATH}")),
        OsString::from("-c"),
        OsString::from(fsmonitor.git_config_arg()),
        OsString::from("-c"),
        OsString::from("core.quotePath=true"),
    ];
    if let Some(safe_directory) = safe_directory {
        command_args.push(OsString::from("-c"));
        let mut safe_directory_arg = OsString::from("safe.directory=");
        safe_directory_arg.push(safe_directory.as_os_str());
        command_args.push(safe_directory_arg);
    }
    command_args.extend(args.iter().cloned());
    command_args
}

pub(crate) fn is_dubious_ownership_stderr(status_success: bool, stderr: &[u8]) -> bool {
    if status_success {
        return false;
    }
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    stderr.contains("detected dubious ownership") && stderr.contains("safe.directory")
}

async fn run_git_command_attempt(
    git: &Path,
    args: &[OsString],
    cwd: &Path,
    fsmonitor: crate::FsmonitorOverride,
    stdout_limit: usize,
) -> Option<std::process::Output> {
    let mut command = Command::new(git);
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let result = timeout(GIT_COMMAND_TIMEOUT, run_git_child(command, fsmonitor, stdout_limit)).await;

    match result {
        Ok(Ok(output)) => Some(output),
        _ => None, // Timeout or error
    }
}

/// Runs the prepared Git child inside a Windows Job so descendants such as
/// clean filters and remote helpers end with the command on timeout or
/// cancellation. Other platforms keep the plain kill-on-drop child.
#[cfg(windows)]
async fn run_git_child(
    mut command: Command,
    fsmonitor: crate::FsmonitorOverride,
    stdout_limit: usize,
) -> std::io::Result<std::process::Output> {
    command.creation_flags(codex_utils_pty::WINDOWS_CREATE_SUSPENDED);
    let managed = codex_utils_pty::ManagedRootProcess::reserve_with_reclaim().await?;
    managed.require_descendant_containment()?;
    // Git can launch clean/process filters even when hooks, external diffs,
    // and text conversion are disabled. Attach the root before it can run,
    // and retain ownership across both output collection and cancellation.
    let (managed, mut child, stdout, stderr) = tokio::task::spawn_blocking(move || {
        // Windows child-pipe reads use Tokio's non-cancellable blocking pool.
        // A cancelled metadata probe can otherwise leave those reads waiting
        // through runtime shutdown. Anonymous temporary files have no pipe
        // writers to await, and are discarded with this operation.
        let stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        command
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?);
        let child = command.spawn()?;
        Ok::<_, std::io::Error>((managed, child, stdout, stderr))
    })
    .await
    .map_err(std::io::Error::other)??;
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("missing Git process id"))?;
    if let Err(error) = managed.attach_and_resume(pid) {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(error);
    }
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = tokio::time::sleep(TokioDuration::from_millis(10)) => {
                crate::operations::check_git_output_limits(&stdout, &stderr, stdout_limit)?;
            }
        }
    };
    let output = tokio::task::spawn_blocking(move || {
        crate::operations::read_git_output(stdout, stderr, status, stdout_limit)
    })
    .await
    .map_err(std::io::Error::other)??;
    if fsmonitor == crate::FsmonitorOverride::BuiltIn
        && output
            .status
            .code()
            .is_some_and(|code| code == 0 || code == 1)
    {
        // A completed inspection may intentionally have started Git's
        // built-in fsmonitor daemon. Timeout and cancellation never reach
        // this transfer of ownership.
        managed.preserve_descendants()?;
    }
    Ok(output)
}

#[cfg(not(windows))]
async fn run_git_child(
    mut command: Command,
    _fsmonitor: crate::FsmonitorOverride,
    _stdout_limit: usize,
) -> std::io::Result<std::process::Output> {
    command.output().await
}

async fn get_git_remotes(cwd: &Path) -> Option<Vec<String>> {
    let output = run_git_command_with_timeout(&["remote"], cwd).await?;
    if !output.status.success() {
        return None;
    }
    let mut remotes: Vec<String> = String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .map(str::to_string)
        .collect();
    if let Some(pos) = remotes.iter().position(|r| r == "origin") {
        let origin = remotes.remove(pos);
        remotes.insert(0, origin);
    }
    Some(remotes)
}

/// Attempt to determine the repository's default branch name.
///
/// Preference order:
/// 1) The symbolic ref at `refs/remotes/<remote>/HEAD` for the first remote (origin prioritized)
/// 2) Local fallback to existing `main` or `master` if present
async fn get_default_branch(cwd: &Path, remotes: &[String]) -> Option<String> {
    // Prefer the first remote (with origin prioritized)
    for remote in remotes {
        // Try symbolic-ref, which returns something like: refs/remotes/origin/main
        if let Some(symref_output) = run_git_command_with_timeout(
            &[
                "symbolic-ref",
                "--quiet",
                &format!("refs/remotes/{remote}/HEAD"),
            ],
            cwd,
        )
        .await
            && symref_output.status.success()
            && let Ok(sym) = String::from_utf8(symref_output.stdout)
        {
            let trimmed = sym.trim();
            if let Some(name) = trimmed.strip_prefix(&format!("refs/remotes/{remote}/"))
                && !name.is_empty()
                && name != "HEAD"
            {
                return Some(name.to_string());
            }
        }
    }

    // No remote-derived default; try common local defaults if they exist
    get_default_branch_local(cwd).await
}

/// Determine the repository's default branch name, if available.
///
/// This inspects remote configuration first (including the symbolic `HEAD`
/// reference) and falls back to common local defaults such as `main` or
/// `master`. Returns `None` when the information cannot be determined, for
/// example when the current directory is not inside a Git repository.
pub async fn default_branch_name(cwd: &Path) -> Option<String> {
    let remotes = get_git_remotes(cwd).await.unwrap_or_default();
    get_default_branch(cwd, &remotes).await
}

/// Attempt to determine the repository's default branch name from local branches.
async fn get_default_branch_local(cwd: &Path) -> Option<String> {
    for candidate in ["main", "master"] {
        if let Some(verify) = run_git_command_with_timeout(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ],
            cwd,
        )
        .await
            && verify.status.success()
        {
            return Some(candidate.to_string());
        }
    }

    None
}

/// Build an ancestry of branches starting at the current branch and ending at the
/// repository's default branch (if determinable)..
async fn branch_ancestry(cwd: &Path, remotes: &[String]) -> Option<Vec<String>> {
    // Discover current branch (ignore detached HEAD by treating it as None)
    let current_branch = run_git_command_with_timeout(&["rev-parse", "--abbrev-ref", "HEAD"], cwd)
        .await
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| s != "HEAD");

    // Discover default branch
    let default_branch = get_default_branch(cwd, remotes).await;

    let mut ancestry: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(cb) = current_branch.clone() {
        seen.insert(cb.clone());
        ancestry.push(cb);
    }
    if let Some(db) = default_branch
        && !seen.contains(&db)
    {
        seen.insert(db.clone());
        ancestry.push(db);
    }

    // Expand candidates: include any remote branches that already contain HEAD.
    // This addresses cases where we're on a new local-only branch forked from a
    // remote branch that isn't the repository default. We prioritize remotes in
    // the order returned by get_git_remotes (origin first).
    for remote in remotes {
        if let Some(output) = run_git_command_with_timeout(
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "--contains=HEAD",
                &format!("refs/remotes/{remote}"),
            ],
            cwd,
        )
        .await
            && output.status.success()
            && let Ok(text) = String::from_utf8(output.stdout)
        {
            for line in text.lines() {
                let short = line.trim();
                // Expect format like: "origin/feature"; extract the branch path after "remote/"
                if let Some(stripped) = short.strip_prefix(&format!("{remote}/"))
                    && !stripped.is_empty()
                    && stripped != "HEAD"
                    && !seen.contains(stripped)
                {
                    seen.insert(stripped.to_string());
                    ancestry.push(stripped.to_string());
                }
            }
        }
    }

    // Ensure we return Some vector, even if empty, to allow caller logic to proceed
    Some(ancestry)
}

// Helper for a single branch: return its merge base with the first matching
// remote ref and the number of local commits since that base. The first item
// is None if the branch is not present on any remote. Returns None if Git could
// not be queried because of process errors or timeouts.
async fn branch_merge_base_and_distance(
    cwd: &Path,
    branch: &str,
    remotes: &[String],
) -> Option<(Option<GitSha>, usize)> {
    if branch == "HEAD" {
        return Some((None, 0));
    }

    // Try the first remote ref that exists for this branch (origin prioritized
    // by the caller) and shares history with HEAD.
    for remote in remotes {
        let remote_ref = format!("refs/remotes/{remote}/{branch}");
        // merge-base already rejects missing refs and unrelated histories.
        // Avoid a separate process that verifies the same ref first.
        let merge_base_output =
            run_git_command_with_timeout(&["merge-base", "HEAD", &remote_ref], cwd).await?;
        if !merge_base_output.status.success() {
            continue;
        }
        let Ok(merge_base_text) = String::from_utf8(merge_base_output.stdout) else {
            return None;
        };
        let merge_base = GitSha::new(merge_base_text.trim());

        let range = format!("{}..HEAD", merge_base.0);
        let count_output =
            run_git_command_with_timeout(&["rev-list", "--count", &range], cwd).await?;
        if !count_output.status.success() {
            return None;
        }
        let Ok(distance_text) = String::from_utf8(count_output.stdout) else {
            return None;
        };
        let Ok(distance) = distance_text.trim().parse::<usize>() else {
            return None;
        };

        return Some((Some(merge_base), distance));
    }

    Some((None, 0))
}

// Finds the closest merge base between HEAD and any matching remote branch.
async fn find_closest_sha(cwd: &Path, branches: &[String], remotes: &[String]) -> Option<GitSha> {
    // A sha and how many commits away from HEAD it is.
    let mut closest_sha: Option<(GitSha, usize)> = None;
    for branch in branches {
        let Some((maybe_remote_sha, distance)) =
            branch_merge_base_and_distance(cwd, branch, remotes).await
        else {
            continue;
        };
        let Some(remote_sha) = maybe_remote_sha else {
            // Preserve existing behavior: skip branches that are not present on a remote.
            continue;
        };
        // Distances cannot be negative, and ties retain the first branch.
        // No later candidate can improve a zero-distance result.
        if distance == 0 {
            return Some(remote_sha);
        }
        match &closest_sha {
            None => closest_sha = Some((remote_sha, distance)),
            Some((_, best_distance)) if distance < *best_distance => {
                closest_sha = Some((remote_sha, distance));
            }
            _ => {}
        }
    }
    closest_sha.map(|(sha, _)| sha)
}

async fn diff_against_sha(cwd: &Path, sha: &GitSha) -> Option<String> {
    let git = crate::git_executable_async().await;
    let fsmonitor = detect_local_fsmonitor_override(git, cwd).await;
    let diff_args = ["diff", "--no-textconv", "--no-ext-diff", &sha.0]
        .into_iter().map(OsString::from).collect::<Vec<_>>();
    let untracked_args = ["ls-files", "-z", "--others", "--exclude-standard"]
        .into_iter().map(OsString::from).collect::<Vec<_>>();
    // Independent read-only prerequisites share the existing bounded runner.
    // Both complete before ordered patch assembly; neither observation is cached.
    let (output, untracked_output) = tokio::join!(
        run_git_command_with_timeout_os_from_limited(git, &diff_args, cwd, fsmonitor, MAX_DIFF_OUTPUT_BYTES),
        run_git_command_with_timeout_os_from_limited(git, &untracked_args, cwd, fsmonitor, MAX_DIFF_OUTPUT_BYTES),
    );
    let output = output?;
    // 0 is success and no diff.
    // 1 is success but there is a diff.
    let exit_ok = output.status.code().is_some_and(|c| c == 0 || c == 1);
    if !exit_ok {
        return None;
    }
    if output.stdout.len() > MAX_DIFF_OUTPUT_BYTES {
        return None;
    }
    let mut diff = String::from_utf8(output.stdout).ok()?;

    let untracked_output = untracked_output?;
    if !untracked_output.status.success() {
        return None;
    }
    if untracked_output.stdout.len() > MAX_DIFF_OUTPUT_BYTES {
        return None;
    }
    let untracked = parse_nul_terminated_paths(untracked_output.stdout)?;

    if !untracked.is_empty() {
        // Use platform-appropriate null device and guard paths with `--`.
        let null_device = OsString::from("NUL");
        let futures = untracked.into_iter().map(|file| {
            let args = vec![
                OsString::from("diff"),
                OsString::from("--no-textconv"),
                OsString::from("--no-ext-diff"),
                OsString::from("--binary"),
                OsString::from("--no-index"),
                // -- ensures that filenames that start with - are not treated as options.
                OsString::from("--"),
                null_device.clone(),
                file,
            ];
            async move {
                run_git_command_with_timeout_os_from_limited(
                    git, &args, cwd, fsmonitor, MAX_DIFF_OUTPUT_BYTES,
                ).await
            }
        });
        let mut results = stream::iter(futures).buffered(UNTRACKED_DIFF_CONCURRENCY);
        while let Some(extra) = results.next().await {
            let extra = extra?;
            if !extra
                .status
                .code()
                .is_some_and(|code| code == 0 || code == 1)
            {
                return None;
            }
            let new_len = diff.len().checked_add(extra.stdout.len())?;
            if new_len > MAX_DIFF_OUTPUT_BYTES {
                return None;
            }
            diff.push_str(&String::from_utf8(extra.stdout).ok()?);
        }
    }

    Some(diff)
}

fn parse_nul_terminated_paths(output: Vec<u8>) -> Option<Vec<OsString>> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(os_string_from_git_bytes)
        .collect()
}

fn os_string_from_git_bytes(bytes: &[u8]) -> Option<OsString> {
    String::from_utf8(bytes.to_vec()).ok().map(OsString::from)
}

/// Resolve the path that should be used for trust checks. Similar to
/// `[get_git_repo_root]`, but resolves to the root of the main
/// repository. Handles worktrees via filesystem inspection without invoking
/// the `git` executable.
pub async fn resolve_root_git_project_for_trust(
    fs: &dyn ExecutorFileSystem,
    cwd: &AbsolutePathBuf,
) -> Option<AbsolutePathBuf> {
    let repo_root = get_git_repo_root_with_fs(fs, cwd).await?;
    resolve_root_git_project_for_trust_from_checkout(fs, &repo_root).await
}

/// Resolve trust identity for a checkout root already discovered by the caller.
/// This performs the same linked-worktree interpretation without walking cwd again.
pub async fn resolve_root_git_project_for_trust_from_checkout(
    fs: &dyn ExecutorFileSystem,
    repo_root: &AbsolutePathBuf,
) -> Option<AbsolutePathBuf> {
    let dot_git = repo_root.join(".git");
    let dot_git_uri = PathUri::from_abs_path(&dot_git);
    if fs
        .get_metadata(&dot_git_uri, /*sandbox*/ None)
        .await
        .ok()?
        .is_directory
    {
        return Some(repo_root.clone());
    }

    let git_dir_s = fs
        .read_file_text(&dot_git_uri, /*sandbox*/ None)
        .await
        .ok()?;
    let git_dir_rel = git_dir_s.trim().strip_prefix("gitdir:")?.trim();
    if git_dir_rel.is_empty() {
        return None;
    }

    // Resolve pointer text using the selected target's native convention.
    let git_dir_uri = PathUri::from_abs_path(repo_root).join(git_dir_rel).ok()?;
    let worktrees_dir = git_dir_uri.parent()?;
    if worktrees_dir.encoded_path().trim_end_matches('/').rsplit('/').next() != Some("worktrees") {
        return None;
    }

    let common_uri = worktrees_dir.parent()?;
    if common_uri.encoded_path().trim_end_matches('/').rsplit('/').next() != Some(".git") {
        return None;
    }
    for directory in [&git_dir_uri, &common_uri] {
        if !fs.get_metadata(directory, None).await.ok()?.is_directory {
            return None;
        }
    }
    // The path shape alone is not authority. Authenticate both the worktree
    // backlink and its common directory on the selected filesystem.
    let backlink = fs.read_file_text(&git_dir_uri.join("gitdir").ok()?, None).await.ok()?;
    let backlink = git_dir_uri.join(backlink.trim()).ok()?;
    if fs.canonicalize(&backlink, None).await.ok()?
        != fs.canonicalize(&dot_git_uri, None).await.ok()?
    {
        return None;
    }
    // A symlink to another checkout's .git file has the same marker identity,
    // but does not make this directory that worktree. Authenticate its parent
    // separately, while still accepting aliases of the whole checkout.
    let backlink_root = backlink.parent()?;
    let checkout_root = PathUri::from_abs_path(repo_root);
    if fs.canonicalize(&backlink_root, None).await.ok()?
        != fs.canonicalize(&checkout_root, None).await.ok()?
    {
        return None;
    }
    let commondir = fs.read_file_text(&git_dir_uri.join("commondir").ok()?, None).await.ok()?;
    let commondir = git_dir_uri.join(commondir.trim()).ok()?;
    if fs.canonicalize(&commondir, None).await.ok()?
        != fs.canonicalize(&common_uri, None).await.ok()?
    {
        return None;
    }
    common_uri.parent()?.to_abs_path().ok()
}

fn find_ancestor_git_entry(base_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = base_dir.to_path_buf();

    loop {
        let dot_git = dir.join(".git");
        if dot_git.exists() {
            return Some((dir, dot_git));
        }

        // Pop one component (go up one directory). `pop` returns false when
        // we have reached the filesystem root.
        if !dir.pop() {
            break;
        }
    }

    None
}

/// Returns the current checked out branch name.
pub async fn current_branch_name(cwd: &Path) -> Option<String> {
    let out = run_git_command_with_timeout(&["branch", "--show-current"], cwd).await?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|name| !name.is_empty())
}

