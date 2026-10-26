use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::time::Duration;

use crate::startup_sync::run_git_command_with_timeout;

pub(super) fn git_remote_revision(
    source: &str,
    ref_name: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    if let Some(ref_name) = ref_name
        && is_full_git_sha(ref_name)
    {
        return Ok(ref_name.to_ascii_lowercase());
    }

    let ref_name = ref_name.unwrap_or("HEAD");
    let candidates = if ref_name == "HEAD" || ref_name.starts_with("refs/") {
        vec![ref_name.to_string()]
    } else {
        vec![
            format!("refs/heads/{ref_name}"),
            format!("refs/tags/{ref_name}"),
        ]
    };
    let mut command = git_command();
    command.arg("ls-remote").arg("--").arg(source);
    for candidate in &candidates {
        command.arg(candidate);
        if candidate.starts_with("refs/tags/") {
            command.arg(format!("{candidate}^{{}}"));
        }
    }
    let output =
        run_git_command_with_timeout(&mut command, "git ls-remote marketplace source", timeout)?;
    ensure_git_success(&output, "git ls-remote marketplace source")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let refs = stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(revision, name)| (name, revision))
        .collect::<std::collections::BTreeMap<_, _>>();
    let matches = candidates
        .iter()
        .filter(|candidate| refs.contains_key(candidate.as_str()))
        .collect::<Vec<_>>();
    let selected = match matches.as_slice() {
        [selected] => selected.as_str(),
        [] => return Err("requested marketplace ref was not found".to_string()),
        _ => {
            return Err("ambiguous marketplace ref; specify refs/heads/ or refs/tags/".to_string());
        }
    };
    let revision = refs
        .get(format!("{selected}^{{}}").as_str())
        .or_else(|| refs.get(selected))
        .ok_or_else(|| "requested marketplace ref was not found".to_string())?;
    if !is_full_git_sha(revision) {
        return Err("git ls-remote returned an invalid marketplace revision".to_string());
    }
    Ok(revision.to_ascii_lowercase())
}

pub(crate) fn clone_git_source(
    source: &str,
    ref_name: Option<&str>,
    sparse_paths: &[String],
    destination: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let revision = ref_name
        .map(|ref_name| git_remote_revision(source, Some(ref_name), timeout))
        .transpose()?;
    let git_destination = git_path_arg(destination);
    let mut clone = git_command();
    clone.arg("clone");
    if revision.is_some() || !sparse_paths.is_empty() {
        clone.arg("--no-checkout");
    }
    if !sparse_paths.is_empty() {
        clone.arg("--filter=blob:none");
    }
    clone.arg("--").arg(source).arg(&git_destination);
    let output = run_git_command_with_timeout(&mut clone, "git clone marketplace source", timeout)?;
    ensure_git_success(&output, "git clone marketplace source")?;

    if !sparse_paths.is_empty() {
        let output = run_git_command_with_timeout(
            git_command()
                .arg("-C")
                .arg(&git_destination)
                .args(["sparse-checkout", "set", "--"])
                .args(sparse_paths),
            "git sparse-checkout marketplace source",
            timeout,
        )?;
        ensure_git_success(&output, "git sparse-checkout marketplace source")?;
    }
    if revision.is_some() || !sparse_paths.is_empty() {
        // Verify a commit object before checkout: a path or an option is never a ref.
        let output = run_git_command_with_timeout(
            git_command()
                .arg("-C")
                .arg(&git_destination)
                .args(["rev-parse", "--verify", "--end-of-options"])
                .arg(format!(
                    "{}^{{commit}}",
                    revision.as_deref().unwrap_or("HEAD")
                )),
            "git resolve marketplace commit",
            timeout,
        )?;
        ensure_git_success(&output, "git resolve marketplace commit")?;
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !is_full_git_sha(&commit) {
            return Err("git returned an invalid marketplace commit".to_string());
        }
        let output = run_git_command_with_timeout(
            git_command()
                .arg("-C")
                .arg(&git_destination)
                .args(["checkout", "--detach"])
                .arg(commit)
                .arg("--"),
            "git checkout marketplace ref",
            timeout,
        )?;
        ensure_git_success(&output, "git checkout marketplace ref")?;
    }
    git_worktree_revision(&git_destination, timeout)
}

fn git_worktree_revision(destination: &Path, timeout: Duration) -> Result<String, String> {
    let output = run_git_command_with_timeout(
        git_command()
            .arg("-C")
            .arg(destination)
            .arg("rev-parse")
            .arg("HEAD"),
        "git rev-parse marketplace revision",
        timeout,
    )?;
    ensure_git_success(&output, "git rev-parse marketplace revision")?;

    let revision = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if revision.is_empty() {
        Err("git rev-parse returned empty revision for marketplace source".to_string())
    } else {
        Ok(revision)
    }
}

fn is_full_git_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn git_path_arg(path: &Path) -> PathBuf {
    path.to_str()
        .and_then(strip_windows_verbatim_path_prefix)
        .map(PathBuf::from)
        .unwrap_or_else(|| path.to_path_buf())
}

fn strip_windows_verbatim_path_prefix(path: &str) -> Option<String> {
    let stripped = path.strip_prefix(r"\\?\")?;
    let stripped = stripped
        .strip_prefix(r"UNC\")
        .map(|unc_path| format!(r"\\{unc_path}"))
        .unwrap_or_else(|| stripped.to_string());
    Some(stripped)
}

fn ensure_git_success(output: &Output, context: &str) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!("{context} failed with status {}", output.status))
    } else {
        Err(format!(
            "{context} failed with status {}: {stderr}",
            output.status
        ))
    }
}
