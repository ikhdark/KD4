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
        return Ok(ref_name.to_string());
    }

    let ref_name = ref_name.unwrap_or("HEAD");
    let output = run_git_command_with_timeout(
        git_command().arg("ls-remote").arg(source).arg(ref_name),
        "git ls-remote marketplace source",
        timeout,
    )?;
    ensure_git_success(&output, "git ls-remote marketplace source")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(first_line) = stdout.lines().next() else {
        return Err("git ls-remote returned empty output for marketplace source".to_string());
    };
    let Some((revision, _)) = first_line.split_once('\t') else {
        return Err(format!(
            "unexpected git ls-remote output for marketplace source: {first_line}"
        ));
    };
    let revision = revision.trim();
    if revision.is_empty() {
        return Err("git ls-remote returned empty revision for marketplace source".to_string());
    }
    Ok(revision.to_string())
}

pub(super) fn clone_git_source(
    source: &str,
    ref_name: Option<&str>,
    sparse_paths: &[String],
    destination: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let git_destination = git_path_arg(destination);
    if sparse_paths.is_empty() {
        let output = run_git_command_with_timeout(
            git_command().arg("clone").arg(source).arg(&git_destination),
            "git clone marketplace source",
            timeout,
        )?;
        ensure_git_success(&output, "git clone marketplace source")?;
        if let Some(ref_name) = ref_name {
            let output = run_git_command_with_timeout(
                git_command()
                    .arg("-C")
                    .arg(&git_destination)
                    .arg("checkout")
                    .arg(ref_name),
                "git checkout marketplace ref",
                timeout,
            )?;
            ensure_git_success(&output, "git checkout marketplace ref")?;
        }
        return git_worktree_revision(&git_destination, timeout);
    }

    let output = run_git_command_with_timeout(
        git_command()
            .arg("clone")
            .arg("--filter=blob:none")
            .arg("--no-checkout")
            .arg(source)
            .arg(&git_destination),
        "git clone marketplace source",
        timeout,
    )?;
    ensure_git_success(&output, "git clone marketplace source")?;

    let mut sparse_checkout = git_command();
    sparse_checkout
        .arg("-C")
        .arg(&git_destination)
        .arg("sparse-checkout")
        .arg("set")
        .args(sparse_paths);
    let output = run_git_command_with_timeout(
        &mut sparse_checkout,
        "git sparse-checkout marketplace source",
        timeout,
    )?;
    ensure_git_success(&output, "git sparse-checkout marketplace source")?;

    let output = run_git_command_with_timeout(
        git_command()
            .arg("-C")
            .arg(&git_destination)
            .arg("checkout")
            .arg(ref_name.unwrap_or("HEAD")),
        "git checkout marketplace ref",
        timeout,
    )?;
    ensure_git_success(&output, "git checkout marketplace ref")?;
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
    strip_windows_verbatim_path_prefix(&path.to_string_lossy())
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

#[cfg(test)]
mod tests {
    use super::git_command;
    use super::git_remote_revision;
    use super::is_full_git_sha;
    use super::strip_windows_verbatim_path_prefix;
    use pretty_assertions::assert_eq;
    use std::ffi::OsStr;

    #[test]
    fn plugin_git_large_output_remote_revision() {
        let repo = tempfile::tempdir().expect("repository");
        let run = |args: &[&str]| {
            let output = git_command()
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .expect("Git UTF-8 output")
                .trim()
                .to_string()
        };
        run(&["init"]);
        run(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        let revision = run(&["rev-parse", "HEAD"]);
        let refs = (0..10_000)
            .map(|index| format!("{revision} refs/heads/branch-{index:05}\n"))
            .collect::<String>();
        std::fs::write(repo.path().join(".git/packed-refs"), refs).expect("many remote refs");

        // The real upload-pack process emits far more than a pipe buffer before
        // ls-remote can finish. Exercise the normal marketplace query boundary.
        assert_eq!(
            git_remote_revision(
                repo.path().to_str().expect("repository path"),
                Some("refs/heads/*"),
                std::time::Duration::from_secs(10)
            )
            .expect("query large remote"),
            revision,
        );
    }

    #[test]
    fn full_git_sha_ref_is_already_a_remote_revision() {
        assert!(is_full_git_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_git_sha("main"));
        assert!(!is_full_git_sha("0123456"));
    }

    #[test]
    fn git_command_uses_path_lookup_with_stable_noninteractive_env() {
        let command = git_command();

        assert_eq!(command.get_program(), OsStr::new("git"));
        assert_eq!(
            command_env(&command, "GIT_OPTIONAL_LOCKS"),
            Some(Some(OsStr::new("0")))
        );
        assert_eq!(
            command_env(&command, "GIT_TERMINAL_PROMPT"),
            Some(Some(OsStr::new("0")))
        );
        assert_eq!(command_env(&command, "PATH"), None);
    }

    #[test]
    fn strips_windows_verbatim_disk_prefix_for_git() {
        assert_eq!(
            strip_windows_verbatim_path_prefix(r"\\?\C:\Users\alice\marketplace"),
            Some(r"C:\Users\alice\marketplace".to_string())
        );
    }

    #[test]
    fn strips_windows_verbatim_unc_prefix_for_git() {
        assert_eq!(
            strip_windows_verbatim_path_prefix(r"\\?\UNC\server\share\marketplace"),
            Some(r"\\server\share\marketplace".to_string())
        );
    }

    #[test]
    fn leaves_non_verbatim_path_without_rewrite() {
        assert_eq!(strip_windows_verbatim_path_prefix(r"C:\Users\alice"), None);
    }

    fn command_env<'a>(
        command: &'a std::process::Command,
        name: &str,
    ) -> Option<Option<&'a OsStr>> {
        command
            .get_envs()
            .find(|(key, _)| key == &OsStr::new(name))
            .map(|(_, value)| value)
    }
}
