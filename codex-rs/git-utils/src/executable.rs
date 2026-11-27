use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

static GIT_EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();

/// Resolve Git for Windows' real executable once, without changing inherited PATH.
/// Commands launched by the user and explicitly supplied test executables are unaffected.
pub fn git_executable() -> &'static Path {
    GIT_EXECUTABLE.get_or_init(|| resolve_git_executable().unwrap_or_else(|| PathBuf::from("git")))
}

pub async fn git_executable_async() -> &'static Path {
    if let Some(path) = GIT_EXECUTABLE.get() {
        return path;
    }
    // Resolution must not wait indefinitely for an occupied blocking pool before
    // the caller can enter its own Git-command deadline. Do not cache this fallback.
    let mut task = tokio::task::spawn_blocking(git_executable);
    match tokio::time::timeout(std::time::Duration::from_secs(2), &mut task).await {
        Ok(Ok(path)) => path,
        _ => {
            task.abort();
            Path::new("git")
        }
    }
}

#[cfg(not(windows))]
fn resolve_git_executable() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn resolve_git_executable() -> Option<PathBuf> {
    use std::io::Read;
    use std::io::Seek;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;

    // A bounded file avoids a wrapper holding a pipe open after its root exits.
    let mut output = tempfile::tempfile().ok()?;
    let managed = codex_utils_pty::ManagedRootProcess::reserve().ok()?;
    managed.require_descendant_containment().ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut child = std::process::Command::new("git")
        .arg("--exec-path")
        .stdin(Stdio::null())
        .stdout(output.try_clone().ok()?)
        .stderr(Stdio::null())
        .creation_flags(codex_utils_pty::WINDOWS_CREATE_SUSPENDED)
        .spawn()
        .ok()?;
    let status = if Instant::now() < deadline && managed.attach_and_resume(child.id()).is_ok() {
        loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break None;
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                Err(_) => break None,
            }
        }
    } else {
        None
    };
    if status.is_none() {
        // The root might not have attached. Kill it directly, then close the
        // Job to kill contained descendants without a blocking cleanup wait.
        let _ = child.kill();
        let _ = child.try_wait();
    }
    drop(managed);
    if Instant::now() >= deadline
        || !status.is_some_and(|status| status.success())
        || output.metadata().ok()?.len() > 32 * 1024
    {
        return None;
    }
    output.rewind().ok()?;
    let mut bytes = Vec::new();
    output.take(32 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    let path = executable_from_exec_path(&bytes)?;
    (Instant::now() < deadline).then_some(path)
}

#[cfg(windows)]
fn executable_from_exec_path(output: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(output)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    let directory = Path::new(text);
    if !directory.is_absolute() || text.contains(['\r', '\n']) {
        return None;
    }
    let executable = directory.join("git.exe");
    executable.is_file().then_some(executable)
}

