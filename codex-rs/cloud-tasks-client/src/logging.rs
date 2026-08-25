use chrono::Utc;
use std::io::Write as _;
use std::path::Path;

/// Append a timestamped diagnostic message to the cloud-task error log.
pub fn append_error_log(message: impl AsRef<str>) {
    append_error_log_to(Path::new("error.log"), message.as_ref());
}

fn append_error_log_to(path: &Path, message: &str) {
    let ts = Utc::now().to_rfc3339();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[{ts}] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::append_error_log_to;

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
}
