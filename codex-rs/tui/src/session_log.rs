use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::app_command::AppCommand;
use crate::legacy_core::config::Config;
use serde::Serialize;
use serde_json::json;

use crate::app_event::AppEvent;

static LOGGER: LazyLock<SessionLogger> = LazyLock::new(SessionLogger::new);

// A bounded queue keeps recording lossless without allowing retained events to grow
// indefinitely. Saturated producers apply backpressure; disk I/O itself belongs
// to the writer thread, and shutdown joins it after draining every accepted record.
const SESSION_LOG_QUEUE_CAPACITY: usize = 256;

struct SessionLogWriter {
    sender: mpsc::SyncSender<serde_json::Value>,
    worker: JoinHandle<std::io::Result<()>>,
}

struct SessionLogger {
    writer: Mutex<Option<SessionLogWriter>>,
}

impl SessionLogger {
    fn new() -> Self {
        Self {
            writer: Mutex::new(None),
        }
    }

    async fn open(&self, path: PathBuf) -> std::io::Result<()> {
        if self.is_enabled() {
            return Ok(());
        }
        let writer = tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)?;
            let (sender, receiver) = mpsc::sync_channel(SESSION_LOG_QUEUE_CAPACITY);
            let worker = std::thread::Builder::new()
                .name("codex-session-log".to_string())
                .spawn(move || Self::write_records(file, receiver))?;
            Ok::<_, std::io::Error>(SessionLogWriter { sender, worker })
        })
        .await
        .map_err(std::io::Error::other)??;
        *self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer);
        Ok(())
    }

    fn write_records(
        file: File,
        receiver: mpsc::Receiver<serde_json::Value>,
    ) -> std::io::Result<()> {
        let mut file = BufWriter::new(file);
        for value in receiver {
            serde_json::to_writer(&mut file, &value)?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
        file.flush()
    }

    fn write_json_line(&self, value: serde_json::Value) {
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(writer) = writer.as_ref()
            && writer.sender.send(value).is_err()
        {
            tracing::warn!("session log writer stopped before accepting a record");
        }
    }

    async fn shutdown(&self) -> std::io::Result<()> {
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(SessionLogWriter { sender, worker }) = writer {
            drop(sender);
            tokio::task::spawn_blocking(move || {
                worker
                    .join()
                    .map_err(|_| std::io::Error::other("session log writer panicked"))?
            })
            .await
            .map_err(std::io::Error::other)??;
        }
        Ok(())
    }

    fn is_enabled(&self) -> bool {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

fn now_ts() -> String {
    // RFC3339 for readability; consumers can parse as needed.
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) async fn maybe_init(config: &Config) {
    let enabled = std::env::var("CODEX_TUI_RECORD_SESSION")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    if !enabled {
        return;
    }

    let path = if let Ok(path) = std::env::var("CODEX_TUI_SESSION_LOG_PATH") {
        PathBuf::from(path)
    } else {
        let mut p = config.log_dir.clone();
        let filename = format!(
            "session-{}.jsonl",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        );
        p.push(filename);
        p
    };

    if let Err(e) = LOGGER.open(path.clone()).await {
        tracing::error!("failed to open session log {:?}: {}", path, e);
        return;
    }

    // Write a header record so we can attach context.
    let header = json!({
        "ts": now_ts(),
        "dir": "meta",
        "kind": "session_start",
        "cwd": config.cwd,
        "model": config.model,
        "model_provider_id": config.model_provider_id,
        "model_provider_name": config.model_provider.name,
    });
    LOGGER.write_json_line(header);
}

pub(crate) fn log_inbound_app_event(event: &AppEvent) {
    // Log only if enabled
    if !LOGGER.is_enabled() {
        return;
    }

    match event {
        AppEvent::NewSession => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "new_session",
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::ClearUi => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "clear_ui",
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::InsertHistoryCell(cell) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "insert_history_cell",
                "lines": cell.transcript_lines(u16::MAX).len(),
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::StartFileSearch(query) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "file_search_start",
                "query": query,
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::FileSearchResult { query, matches } => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "file_search_result",
                "query": query,
                "matches": matches.len(),
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::PetPreviewLoaded { request_id, result } => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "PetPreviewLoaded",
                "request_id": request_id,
                "ok": result.is_ok(),
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::PetSelectionLoaded {
            request_id,
            pet_id,
            result,
        } => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "PetSelectionLoaded",
                "request_id": request_id,
                "pet_id": pet_id,
                "ok": result.is_ok(),
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::CodexOp(AppCommand::BugCreate { .. }) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "BugCreate",
            });
            LOGGER.write_json_line(value);
        }
        // Noise or control flow – record variant only
        other => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": format!("{other:?}").split('(').next().unwrap_or("app_event"),
            });
            LOGGER.write_json_line(value);
        }
    }
}

pub(crate) fn log_outbound_op(op: &AppCommand) {
    if !LOGGER.is_enabled() {
        return;
    }
    // Bug reports are deliberately excluded from command/session logs. The
    // durable bug database and isolated classifier request are their only raw
    // text sinks.
    if !outbound_op_is_loggable(op) {
        return;
    }
    write_record("from_tui", "op", op);
}

fn outbound_op_is_loggable(op: &AppCommand) -> bool {
    !matches!(op, AppCommand::BugCreate { .. })
}

pub(crate) async fn log_session_end() {
    if !LOGGER.is_enabled() {
        return;
    }
    let value = json!({
        "ts": now_ts(),
        "dir": "meta",
        "kind": "session_end",
    });
    LOGGER.write_json_line(value);
    if let Err(error) = LOGGER.shutdown().await {
        tracing::warn!("session log shutdown error: {error}");
    }
}

fn write_record<T>(dir: &str, kind: &str, obj: &T)
where
    T: Serialize,
{
    let value = json!({
        "ts": now_ts(),
        "dir": dir,
        "kind": kind,
        "payload": obj,
    });
    LOGGER.write_json_line(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_logger_shutdown_drains_records_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/session.jsonl");
        let logger = SessionLogger::new();
        logger.open(path.clone()).await.unwrap();
        // Exceed the queue capacity to cover lossless backpressure as well as
        // escaping and the final records still queued when shutdown starts.
        for sequence in 0..(SESSION_LOG_QUEUE_CAPACITY * 2 + 1) {
            logger.write_json_line(json!({"sequence": sequence, "text": "first\nsecond"}));
        }
        logger.shutdown().await.unwrap();
        assert!(!logger.is_enabled());
        let records: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let expected: Vec<_> = (0..513)
            .map(|sequence| json!({"sequence": sequence, "text": "first\nsecond"}))
            .collect();
        assert_eq!(records, expected);
        logger.write_json_line(json!({"after_shutdown": true}));
        logger.shutdown().await.unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 513);
    }

    #[tokio::test]
    async fn session_logger_open_failure_leaves_recording_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let occupied = directory.path().join("occupied");
        std::fs::write(&occupied, "keep existing file").unwrap();
        let logger = SessionLogger::new();
        assert!(logger.open(occupied.join("session.jsonl")).await.is_err());
        assert!(!logger.is_enabled());
        logger.write_json_line(json!({"not_recorded": true}));
        logger.shutdown().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(occupied).unwrap(),
            "keep existing file"
        );
    }

    #[test]
    fn bug_report_text_is_excluded_from_outbound_session_logs() {
        let op = AppCommand::bug_create("private report text".to_string());

        assert!(!outbound_op_is_loggable(&op));
    }
}
