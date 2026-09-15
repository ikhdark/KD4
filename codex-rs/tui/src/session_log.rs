use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::thread::JoinHandle;
use tokio::sync::mpsc;

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
    sender: mpsc::Sender<serde_json::Value>,
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

    async fn open(&self, path: PathBuf, create_new: bool) -> std::io::Result<()> {
        if self.is_enabled() {
            return Ok(());
        }
        let writer = tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .create_new(create_new)
                .truncate(!create_new)
                .write(true)
                .open(path)?;
            let (sender, receiver) = mpsc::channel(SESSION_LOG_QUEUE_CAPACITY);
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
        mut receiver: mpsc::Receiver<serde_json::Value>,
    ) -> std::io::Result<()> {
        let mut file = BufWriter::new(file);
        while let Some(value) = receiver.blocking_recv() {
            serde_json::to_writer(&mut file, &value)?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
        file.flush()
    }

    async fn write_json_line(&self, value: serde_json::Value) {
        let sender = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|writer| writer.sender.clone());
        if let Some(sender) = sender
            && sender.send(value).await.is_err()
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

    let (path, create_new) = if let Ok(path) = std::env::var("CODEX_TUI_SESSION_LOG_PATH") {
        (PathBuf::from(path), false)
    } else {
        let mut p = config.log_dir.clone();
        let filename = format!(
            "session-{}-{}.jsonl",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            uuid::Uuid::new_v4()
        );
        p.push(filename);
        (p, true)
    };

    if let Err(e) = LOGGER.open(path.clone(), create_new).await {
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
    LOGGER.write_json_line(header).await;
}

pub(crate) async fn log_inbound_app_event(event: &AppEvent) {
    // Log only if enabled
    if !LOGGER.is_enabled() {
        return;
    }

    LOGGER
        .write_json_line(inbound_app_event_record(event))
        .await;
}

fn inbound_app_event_record(event: &AppEvent) -> serde_json::Value {
    match event {
        AppEvent::NewSession => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "new_session",
            })
        }
        AppEvent::ClearUi => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "clear_ui",
            })
        }
        AppEvent::InsertHistoryCell(_) => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "insert_history_cell",
            })
        }
        AppEvent::StartFileSearch(query) => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "file_search_start",
                "query": query,
            })
        }
        AppEvent::FileSearchResult { query, matches } => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "file_search_result",
                "query": query,
                "matches": matches.len(),
            })
        }
        AppEvent::PetPreviewLoaded { request_id, result } => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "PetPreviewLoaded",
                "request_id": request_id,
                "ok": result.is_ok(),
            })
        }
        AppEvent::PetSelectionLoaded {
            request_id,
            pet_id,
            result,
        } => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "PetSelectionLoaded",
                "request_id": request_id,
                "pet_id": pet_id,
                "ok": result.is_ok(),
            })
        }
        AppEvent::CodexOp(AppCommand::BugCreate { .. }) => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": "BugCreate",
            })
        }
        // Noise or control flow – record variant only
        other => {
            json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": <&'static str>::from(other),
            })
        }
    }
}

pub(crate) async fn log_outbound_op(op: &AppCommand) {
    if !LOGGER.is_enabled() {
        return;
    }
    // Bug reports are deliberately excluded from command/session logs. The
    // durable bug database and isolated classifier request are their only raw
    // text sinks.
    if !outbound_op_is_loggable(op) {
        return;
    }
    write_record("from_tui", "op", op).await;
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
    LOGGER.write_json_line(value).await;
    if let Err(error) = LOGGER.shutdown().await {
        tracing::warn!("session log shutdown error: {error}");
    }
}

async fn write_record<T>(dir: &str, kind: &str, obj: &T)
where
    T: Serialize,
{
    let value = json!({
        "ts": now_ts(),
        "dir": dir,
        "kind": kind,
        "payload": obj,
    });
    LOGGER.write_json_line(value).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_event_logs_only_the_variant() {
        let record = inbound_app_event_record(&AppEvent::ClearUiAndSubmitUserMessage {
            text: "private prompt".to_string(),
        });
        assert_eq!(record["variant"], "ClearUiAndSubmitUserMessage");
        assert_eq!(record.as_object().unwrap().len(), 4);
        assert!(!record.to_string().contains("private prompt"));
    }

    #[test]
    fn history_cell_logging_does_not_render_the_transcript() {
        #[derive(Debug)]
        struct UnrenderableCell;
        impl crate::history_cell::HistoryCell for UnrenderableCell {
            fn raw_lines(&self) -> Vec<ratatui::text::Line<'static>> {
                panic!("logging must not render raw history");
            }

            fn display_lines(&self, _width: u16) -> Vec<ratatui::text::Line<'static>> {
                panic!("logging must not render history");
            }
        }
        let record =
            inbound_app_event_record(&AppEvent::InsertHistoryCell(Box::new(UnrenderableCell)));
        assert_eq!(record["kind"], "insert_history_cell");
        assert_eq!(record.as_object().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn automatically_named_logs_never_truncate_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, b"existing session").unwrap();
        let logger = SessionLogger::new();
        let error = logger.open(path.clone(), true).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(!logger.is_enabled());
        assert_eq!(std::fs::read(path).unwrap(), b"existing session");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_session_logger_yields_and_preserves_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let file = File::create(&path).unwrap();
        let (sender, receiver) = mpsc::channel(1);
        let (release, resume) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            resume
                .recv_timeout(std::time::Duration::from_secs(5))
                .map_err(std::io::Error::other)?;
            SessionLogger::write_records(file, receiver)
        });
        let logger = SessionLogger {
            writer: Mutex::new(Some(SessionLogWriter { sender, worker })),
        };
        logger.write_json_line(json!({"sequence": 0})).await;
        let pending = logger.write_json_line(json!({"sequence": 1}));
        tokio::pin!(pending);
        tokio::select! {
            () = &mut pending => panic!("full log queue must apply backpressure"),
            () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }
        assert!(
            logger.is_enabled(),
            "pending send must not retain the state lock"
        );
        release.send(()).unwrap();
        pending.await;
        logger.shutdown().await.unwrap();
        let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records,
            vec![json!({"sequence": 0}), json!({"sequence": 1})]
        );
    }

    #[tokio::test]
    async fn session_logger_shutdown_drains_records_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/session.jsonl");
        let logger = SessionLogger::new();
        logger.open(path.clone(), false).await.unwrap();
        // Exceed the queue capacity to cover lossless backpressure as well as
        // escaping and the final records still queued when shutdown starts.
        for sequence in 0..(SESSION_LOG_QUEUE_CAPACITY * 2 + 1) {
            logger
                .write_json_line(json!({"sequence": sequence, "text": "first\nsecond"}))
                .await;
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
        logger
            .write_json_line(json!({"after_shutdown": true}))
            .await;
        logger.shutdown().await.unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 513);
    }

    #[tokio::test]
    async fn session_logger_open_failure_leaves_recording_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let occupied = directory.path().join("occupied");
        std::fs::write(&occupied, "keep existing file").unwrap();
        let logger = SessionLogger::new();
        assert!(
            logger
                .open(occupied.join("session.jsonl"), false)
                .await
                .is_err()
        );
        assert!(!logger.is_enabled());
        logger.write_json_line(json!({"not_recorded": true})).await;
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
