use chrono::Utc;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

pub(crate) const ERROR_LOG_FILE_NAME: &str = "codex-cloud-tasks.log";

static ERROR_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Sends the cloud-task diagnostic log to `log_dir` (normally `$CODEX_HOME/log`); the first
/// call wins. Until then messages are dropped rather than written into the working directory,
/// which is usually the user's repository.
pub fn set_error_log_dir(log_dir: &Path) {
    let _ = std::fs::create_dir_all(log_dir);
    let _ = ERROR_LOG_PATH.set(log_dir.join(ERROR_LOG_FILE_NAME));
}

/// Append a timestamped diagnostic message to the cloud-task log.
pub fn append_error_log(message: impl AsRef<str>) {
    if let Some(path) = ERROR_LOG_PATH.get() {
        append_error_log_to(path, message.as_ref());
    }
}

fn append_error_log_to(path: &Path, message: &str) {
    let ts = Utc::now().to_rfc3339();
    // Write each entry in one append; `writeln!` on an unbuffered file issues a write per
    // formatted piece, which concurrent tasks interleave within a line.
    let entry = format!("[{ts}] {message}\n");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(entry.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::ERROR_LOG_FILE_NAME;
    use super::append_error_log;
    use super::append_error_log_to;
    use super::set_error_log_dir;

    #[test]
    fn appends_timestamped_messages() {
        let path =
            std::env::temp_dir().join(format!("codex-cloud-tasks-log-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        append_error_log_to(&path, "first");
        append_error_log_to(&path, "second");

        let contents = std::fs::read_to_string(&path).expect("read log");
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with('['));
        assert!(lines[0].ends_with("] first"));
        assert!(lines[1].ends_with("] second"));

        std::fs::remove_file(path).expect("remove log");
    }

    #[test]
    fn messages_are_written_only_to_the_configured_log_directory() {
        let home = tempfile::tempdir().expect("temporary home");
        let log_dir = home.path().join("log");

        append_error_log("before configuration");
        set_error_log_dir(&log_dir);
        append_error_log("after configuration");

        let contents =
            std::fs::read_to_string(log_dir.join(ERROR_LOG_FILE_NAME)).expect("configured log");
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{contents}");
        assert!(lines[0].ends_with("] after configuration"), "{contents}");
    }
}
