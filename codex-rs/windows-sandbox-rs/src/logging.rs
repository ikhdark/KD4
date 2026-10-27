use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_appender::rolling::RollingFileAppender;
use tracing_appender::rolling::Rotation;

const LOG_COMMAND_PREVIEW_LIMIT: usize = 200;
pub const LOG_FILE_PREFIX: &str = "sandbox";
pub const LOG_FILE_SUFFIX: &str = "log";
pub const MAX_LOG_FILES: usize = 90;

fn exe_label() -> &'static str {
    static LABEL: OnceLock<String> = OnceLock::new();
    LABEL.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "proc".to_string())
    })
}

fn preview(command: &[String]) -> String {
    let joined = command.join(" ");
    if joined.len() <= LOG_COMMAND_PREVIEW_LIMIT {
        joined
    } else {
        const MARKER: &str = " … ";
        let budget = LOG_COMMAND_PREVIEW_LIMIT - MARKER.len();
        let prefix_end = joined.floor_char_boundary(budget / 2);
        let suffix_start = joined.ceil_char_boundary(joined.len() - (budget - budget / 2));
        format!(
            "{}{MARKER}{}",
            &joined[..prefix_end],
            &joined[suffix_start..]
        )
    }
}

pub fn log_file_path_for_utc_date(base_dir: &Path, date: chrono::NaiveDate) -> PathBuf {
    base_dir.join(format!(
        "{LOG_FILE_PREFIX}.{}.{}",
        date.format("%Y-%m-%d"),
        LOG_FILE_SUFFIX
    ))
}

pub fn current_log_file_path(base_dir: &Path) -> PathBuf {
    log_file_path_for_utc_date(base_dir, chrono::Utc::now().date_naive())
}

pub fn current_log_file_path_for_codex_home(codex_home: &Path) -> PathBuf {
    current_log_file_path(&crate::sandbox_dir(codex_home))
}

pub fn log_writer(base_dir: &Path) -> Option<RollingFileAppender> {
    if !base_dir.is_dir() {
        return None;
    }

    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix(LOG_FILE_SUFFIX)
        .max_log_files(MAX_LOG_FILES)
        .build(base_dir)
        .ok()
}

fn append_line(line: &str, base_dir: Option<&Path>) {
    if let Some(dir) = base_dir
        && let Some(mut f) = log_writer(dir)
    {
        let _ = writeln!(f, "{line}");
    }
}

pub fn log_start(command: &[String], base_dir: Option<&Path>) {
    let p = preview(command);
    log_note(&format!("START: {p}"), base_dir);
}

pub fn log_success(command: &[String], base_dir: Option<&Path>) {
    let p = preview(command);
    log_note(&format!("SUCCESS: {p}"), base_dir);
}

pub fn log_failure(command: &[String], detail: &str, base_dir: Option<&Path>) {
    let p = preview(command);
    log_note(&format!("FAILURE: {p} ({detail})"), base_dir);
}

// Debug logging helper. Emits only when SBX_DEBUG=1 to avoid noisy logs.
pub fn debug_log(msg: &str, base_dir: Option<&Path>) {
    if std::env::var("SBX_DEBUG").ok().as_deref() == Some("1") {
        append_line(&format!("DEBUG: {msg}"), base_dir);
        eprintln!("{msg}");
    }
}

/// Opt-in content-free process/policy receipts, independent of command debug
/// logging. Lazy formatting keeps default launches free of diagnostic queries.
pub fn custody_diagnostic(message: impl FnOnce() -> String, base_dir: Option<&Path>) {
    if std::env::var("CODEX_SANDBOX_CUSTODY_DIAGNOSTICS").ok().as_deref() == Some("1") {
        static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
        let elapsed = ORIGIN.get_or_init(std::time::Instant::now).elapsed().as_micros();
        let message = format!("sandbox_custody observer_pid={} t_us={} {}", std::process::id(), elapsed, message());
        append_line(&message, base_dir);
        eprintln!("{message}");
    }
}

// Unconditional note logging to the daily sandbox log.
pub fn log_note(msg: &str, base_dir: Option<&Path>) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    append_line(&format!("[{ts} {}] {}", exe_label(), msg), base_dir);
}

