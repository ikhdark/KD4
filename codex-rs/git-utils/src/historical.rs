use std::path::PathBuf;
use std::process::Command;

use crate::operations::GitCommandControl;
use crate::operations::run_git_output;

pub(crate) struct ReadCancellation(pub(crate) GitCommandControl);

impl Drop for ReadCancellation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) fn relative_git_path(path: &str) -> Result<String, String> {
    let path = path.replace('\\', "/");
    if path.is_empty()
        || path.contains(['\0', ':', '\n', '\r'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err("Git path must be repository-root-relative, without empty, . or .. components".into());
    }
    Ok(path)
}

pub(crate) fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn read_git(
    dir: &std::path::Path,
    args: &[&str],
    limit: usize,
    control: &GitCommandControl,
) -> Result<String, String> {
    String::from_utf8(read_git_bytes(dir, args, limit, control)?)
        .map_err(|_| "Git read requires UTF-8 text".into())
}

/// For output whose free-form metadata (author names, subjects) may use a
/// legacy encoding; ASCII structural fields still decode exactly.
pub(crate) fn read_git_lossy(
    dir: &std::path::Path,
    args: &[&str],
    limit: usize,
    control: &GitCommandControl,
) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&read_git_bytes(dir, args, limit, control)?).into_owned())
}

fn read_git_bytes(
    dir: &std::path::Path,
    args: &[&str],
    limit: usize,
    control: &GitCommandControl,
) -> Result<Vec<u8>, String> {
    let output = read_git_command(dir, args, limit, control)?;
    if !output.status.success() {
        return Err(format!("Git read failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(output.stdout)
}

/// The same bounded, hardened read owner, retaining exit status and exact bytes.
pub(crate) fn read_git_command(
    dir: &std::path::Path,
    args: &[&str],
    limit: usize,
    control: &GitCommandControl,
) -> Result<std::process::Output, String> {
    read_git_command_with(dir, args, None, &[], limit, control)
}

/// `index` replaces the live index (GIT_INDEX_FILE) for commands that would
/// otherwise write refreshed stat data back to it. `config` keys are passed
/// verbatim after inherited GIT_CONFIG_KEY_n entries: `-c` splits at the first
/// `=`, which repository-named keys may contain (filter driver `x=y`).
pub(crate) fn read_git_command_with(
    dir: &std::path::Path,
    args: &[&str],
    index: Option<&std::path::Path>,
    config: &[(String, &str)],
    limit: usize,
    control: &GitCommandControl,
) -> Result<std::process::Output, String> {
    let mut command = Command::new(crate::git_executable());
    command.current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "core.fsmonitor=false", "-c"])
        .arg(format!("core.hooksPath={}", crate::DISABLED_HOOKS_PATH));
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    if !config.is_empty() {
        let inherited = std::env::var("GIT_CONFIG_COUNT")
            .ok()
            .and_then(|count| count.parse::<u32>().ok())
            .map_or(0, |count| count as usize);
        for (slot, (key, value)) in (inherited..).zip(config) {
            command
                .env(format!("GIT_CONFIG_KEY_{slot}"), key)
                .env(format!("GIT_CONFIG_VALUE_{slot}"), value);
        }
        command.env("GIT_CONFIG_COUNT", (inherited + config.len()).to_string());
    }
    // No safe.directory override: a repository Git does not trust (e.g. a
    // C:\.git another user planted above cwd) is refused below. Every entry
    // point starts with rev-parse, which fails closed, whereas config,
    // hash-object and diff would silently fall back to non-repository mode.
    command.args(args);
    // The shared owner's cancel/timeout text describes working-file writes.
    let output = run_git_output(command, limit, control).map_err(|error| match error.kind() {
        std::io::ErrorKind::Interrupted => {
            "Git read cancelled; no repository writes were requested".to_string()
        }
        std::io::ErrorKind::TimedOut => {
            "Git read exceeded its 30-second deadline; no repository writes were requested"
                .to_string()
        }
        _ => error.to_string(),
    })?;
    if crate::info::is_dubious_ownership_stderr(output.status.success(), &output.stderr) {
        return Err(format!(
            "Git refused this repository: it is not trusted by Git's safe.directory (dubious ownership), and Git reads never override that. Only the user should add a safe.directory exception, after confirming they trust its owner. Git: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

/// Verbatim config entries that disable every configured clean/process filter
/// driver without running it, for `read_git_command_with`.
pub(crate) fn disabled_filter_config(
    root: &std::path::Path,
    control: &GitCommandControl,
) -> Result<Vec<(String, &'static str)>, String> {
    let config = read_git(root, &["config", "--null", "--list"], 1024 * 1024, control)?;
    Ok(config
        .split_terminator('\0')
        .filter_map(|entry| {
            let key = entry.split('\n').next().unwrap_or("");
            let lower = key.to_ascii_lowercase();
            let value = if lower.ends_with(".required") {
                "false"
            } else if lower.ends_with(".clean") || lower.ends_with(".process") {
                ""
            } else {
                return None;
            };
            lower.starts_with("filter.").then(|| (key.to_string(), value))
        })
        .collect())
}

pub(crate) fn resolve_read_revision(
    root: &std::path::Path,
    revision: &str,
    control: &GitCommandControl,
) -> Result<String, String> {
    if revision.trim().is_empty() || revision.contains(['\0', '\n', '\r']) {
        return Err("revision must be a nonempty Git commit revision".into());
    }
    let oid = read_git(root, &[
        "rev-parse", "--verify", "--end-of-options", &format!("{revision}^{{commit}}"),
    ], 1024, control)?.trim().to_string();
    if !valid_oid(&oid) {
        return Err("Git did not return one immutable commit identity".into());
    }
    Ok(oid)
}

/// Exact, unfiltered blob bytes and their immutable repository identities.
pub struct HistoricalFile {
    pub repository_root: PathBuf,
    pub path: String,
    pub commit_oid: String,
    pub blob_oid: String,
    pub contents: String,
}

/// Resolve a revision once; all subsequent lookups use immutable object IDs.
/// Reuses the bounded Git process/Job owner; dropping this future cancels it.
pub async fn read_historical_file(
    cwd: PathBuf,
    path: String,
    revision: String,
    max_bytes: usize,
) -> Result<HistoricalFile, String> {
    let control = GitCommandControl::new();
    let _cancellation = ReadCancellation(control.clone());
    tokio::task::spawn_blocking(move || {
        let path = relative_git_path(&path)?;
        let run = |dir: &std::path::Path, args: &[&str], limit: usize| -> Result<String, String> {
            read_git(dir, args, limit, &control)
        };
        let root = run(&cwd, &["rev-parse", "--show-toplevel"], 32 * 1024)?;
        let repository_root = PathBuf::from(root.trim_end_matches(['\r', '\n']));
        let commit_oid = resolve_read_revision(&repository_root, &revision, &control)?;
        let entry = run(&repository_root, &[
            "--literal-pathspecs", "ls-tree", "-z", &commit_oid, "--", &path,
        ], 32 * 1024)?;
        let Some((header, name)) = entry.strip_suffix('\0').and_then(|entry| entry.split_once('\t')) else {
            return Err("path is missing at the resolved commit".into());
        };
        let fields = header.split(' ').collect::<Vec<_>>();
        if name != path || fields.len() != 3 || !matches!(fields[0], "100644" | "100755") || fields[1] != "blob" || !valid_oid(fields[2]) {
            return Err("historical read requires one regular file, not a tree, symlink or submodule".into());
        }
        let blob_oid = fields[2].to_string();
        let size = run(&repository_root, &["cat-file", "-s", &blob_oid], 1024)?
            .trim().parse::<usize>().map_err(|_| "invalid Git blob size")?;
        if size > max_bytes {
            return Err(format!("historical blob exceeds the {max_bytes}-byte read limit"));
        }
        let contents = run(&repository_root, &["cat-file", "blob", &blob_oid], max_bytes)?;
        if contents.len() != size {
            return Err("Git blob size changed during capture".into());
        }
        Ok(HistoricalFile { repository_root, path, commit_oid, blob_oid, contents })
    }).await.map_err(|error| error.to_string())?
}
