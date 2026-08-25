//! Tracing log export into the local SQLite log database.
//!
//! This module provides a `tracing_subscriber::Layer` that captures events,
//! formats each one into a `LogEntry`, and sends entries to a bounded background
//! queue. The background task inserts into the dedicated `logs` SQLite database
//! in batches to keep logging overhead low.
//!
//! ## Usage
//!
//! ```no_run
//! use codex_state::log_db;
//! use tracing_subscriber::prelude::*;
//!
//! # async fn example(state_db: std::sync::Arc<codex_state::StateRuntime>) {
//! let layer = log_db::start(state_db);
//! let _ = tracing_subscriber::registry()
//!     .with(layer)
//!     .try_init();
//! # }
//! ```

use std::sync::OnceLock;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::Event;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::level_filters::LevelFilter;
use tracing::span::Attributes;
use tracing::span::Id;
use tracing::span::Record;
use tracing_subscriber::Layer;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::FormattedFields;
use tracing_subscriber::fmt::format::DefaultFields;
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

use crate::LogEntry;
use crate::StateRuntime;
use crate::runtime::LogRetentionScope;

const LOG_QUEUE_CAPACITY: usize = 512;
const LOG_BATCH_SIZE: usize = 128;
const LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

pub fn default_filter() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_target("hyper_util", LevelFilter::WARN)
        .with_target("log", LevelFilter::OFF)
        .with_target("codex_otel.log_only", LevelFilter::OFF)
        .with_target("codex_otel.trace_safe", LevelFilter::OFF)
        .with_target("rmcp::service", LevelFilter::INFO)
        .with_target("codex_core::post_sampling_token_estimate", LevelFilter::OFF)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogSinkQueueConfig {
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub flush_interval: Duration,
}

impl Default for LogSinkQueueConfig {
    fn default() -> Self {
        Self {
            queue_capacity: LOG_QUEUE_CAPACITY,
            batch_size: LOG_BATCH_SIZE,
            flush_interval: LOG_FLUSH_INTERVAL,
        }
    }
}

impl LogSinkQueueConfig {
    fn normalized(self) -> Self {
        Self {
            queue_capacity: self.queue_capacity.max(1),
            batch_size: self.batch_size.max(1),
            flush_interval: if self.flush_interval.is_zero() {
                LOG_FLUSH_INTERVAL
            } else {
                self.flush_interval
            },
        }
    }
}

pub struct LogDbLayer {
    sender: mpsc::Sender<LogDbCommand>,
    process_uuid: String,
}

pub fn start(state_db: std::sync::Arc<StateRuntime>) -> LogDbLayer {
    LogDbLayer::start(state_db)
}

impl Clone for LogDbLayer {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            process_uuid: self.process_uuid.clone(),
        }
    }
}

impl LogDbLayer {
    pub fn start(state_db: std::sync::Arc<StateRuntime>) -> Self {
        Self::start_with_config(state_db, LogSinkQueueConfig::default())
    }

    pub fn start_with_config(
        state_db: std::sync::Arc<StateRuntime>,
        config: LogSinkQueueConfig,
    ) -> Self {
        let config = config.normalized();
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        tokio::spawn(run_inserter(state_db, receiver, config));
        Self {
            sender,
            process_uuid: current_process_log_uuid().to_string(),
        }
    }

    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.sender.send(LogDbCommand::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    fn try_send(&self, entry: LogEntry) {
        let _ = self.sender.try_send(LogDbCommand::Entry(Box::new(entry)));
    }
}

impl<S> Layer<S> for LogDbLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = SpanFieldVisitor::default();
        attrs.record(&mut visitor);

        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanLogContext {
                name: span.metadata().name().to_string(),
                formatted_fields: format_fields(attrs),
                thread_id: visitor.thread_id,
            });
        }
    }

    fn on_record(
        &self,
        id: &Id,
        values: &Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = SpanFieldVisitor::default();
        values.record(&mut visitor);

        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(log_context) = extensions.get_mut::<SpanLogContext>() {
                if let Some(thread_id) = visitor.thread_id {
                    log_context.thread_id = Some(thread_id);
                }
                append_fields(&mut log_context.formatted_fields, values);
            } else {
                extensions.insert(SpanLogContext {
                    name: span.metadata().name().to_string(),
                    formatted_fields: format_fields(values),
                    thread_id: visitor.thread_id,
                });
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let metadata = event.metadata();
        // `tracing-log` checks filters with the original log target before
        // dispatching an event whose tracing target is `log`, so the outer
        // target filter cannot reliably reject these bridged events.
        if metadata.target() == "log" {
            return;
        }

        // The SDK emits DEBUG timer meta-events every second per process; these
        // were over 30% of retained logs in measured high-fanout Codex environments.
        if metadata.target() == "opentelemetry_sdk"
            && matches!(
                *metadata.level(),
                tracing::Level::TRACE | tracing::Level::DEBUG
            )
        {
            return;
        }

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let thread_id = visitor
            .thread_id
            .clone()
            .or_else(|| event_thread_id(event, &ctx));
        let feedback_log_body = format_feedback_log_body(event, &ctx);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));
        let entry = LogEntry {
            ts: now.as_secs() as i64,
            ts_nanos: now.subsec_nanos() as i64,
            level: metadata.level().as_str().to_string(),
            target: metadata.target().to_string(),
            message: visitor.message,
            feedback_log_body: Some(feedback_log_body),
            thread_id,
            process_uuid: Some(self.process_uuid.clone()),
            module_path: metadata.module_path().map(ToString::to_string),
            file: metadata.file().map(ToString::to_string),
            line: metadata.line().map(|line| line as i64),
        };

        self.try_send(entry);
    }
}

enum LogDbCommand {
    Entry(Box<LogEntry>),
    Flush(oneshot::Sender<()>),
}

#[derive(Debug)]
struct SpanLogContext {
    name: String,
    formatted_fields: String,
    thread_id: Option<String>,
}

#[derive(Default)]
struct SpanFieldVisitor {
    thread_id: Option<String>,
}

impl SpanFieldVisitor {
    fn record_field(&mut self, field: &Field, value: String) {
        if field.name() == "thread_id" && self.thread_id.is_none() {
            self.thread_id = Some(value);
        }
    }
}

impl Visit for SpanFieldVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_field(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_field(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_field(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_field(field, value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_field(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_field(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_field(field, format!("{value:?}"));
    }
}

fn event_thread_id<S>(
    event: &Event<'_>,
    ctx: &tracing_subscriber::layer::Context<'_, S>,
) -> Option<String>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let mut thread_id = None;
    if let Some(scope) = ctx.event_scope(event) {
        for span in scope.from_root() {
            let extensions = span.extensions();
            if let Some(log_context) = extensions.get::<SpanLogContext>()
                && log_context.thread_id.is_some()
            {
                thread_id = log_context.thread_id.clone();
            }
        }
    }
    thread_id
}

fn format_feedback_log_body<S>(
    event: &Event<'_>,
    ctx: &tracing_subscriber::layer::Context<'_, S>,
) -> String
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let mut feedback_log_body = String::new();
    if let Some(scope) = ctx.event_scope(event) {
        for span in scope.from_root() {
            let extensions = span.extensions();
            if let Some(log_context) = extensions.get::<SpanLogContext>() {
                feedback_log_body.push_str(&log_context.name);
                if !log_context.formatted_fields.is_empty() {
                    feedback_log_body.push('{');
                    feedback_log_body.push_str(&log_context.formatted_fields);
                    feedback_log_body.push('}');
                }
            } else {
                feedback_log_body.push_str(span.metadata().name());
            }
            feedback_log_body.push(':');
        }
        if !feedback_log_body.is_empty() {
            feedback_log_body.push(' ');
        }
    }
    feedback_log_body.push_str(&format_fields(event));
    feedback_log_body
}

fn format_fields<R>(fields: R) -> String
where
    R: RecordFields,
{
    let formatter = DefaultFields::default();
    let mut formatted = FormattedFields::<DefaultFields>::new(String::new());
    let _ = formatter.format_fields(formatted.as_writer(), fields);
    formatted.fields
}

fn append_fields(fields: &mut String, values: &Record<'_>) {
    let formatter = DefaultFields::default();
    let mut formatted = FormattedFields::<DefaultFields>::new(std::mem::take(fields));
    let _ = formatter.add_fields(&mut formatted, values);
    *fields = formatted.fields;
}

fn current_process_log_uuid() -> &'static str {
    static PROCESS_LOG_UUID: OnceLock<String> = OnceLock::new();
    PROCESS_LOG_UUID.get_or_init(|| {
        let pid = std::process::id();
        let process_uuid = Uuid::new_v4();
        format!("pid:{pid}:{process_uuid}")
    })
}

async fn run_inserter(
    state_db: std::sync::Arc<StateRuntime>,
    mut receiver: mpsc::Receiver<LogDbCommand>,
    config: LogSinkQueueConfig,
) {
    let mut buffer = Vec::with_capacity(config.batch_size);
    let mut ticker = tokio::time::interval(config.flush_interval);
    let mut pending_retention = Some(LogRetentionScope::for_reconciliation());
    let mut maintenance = None;
    let mut retry_on_tick = false;
    // Consume the immediate startup tick so entries flush after the interval.
    ticker.tick().await;
    start_retention_maintenance(&state_db, &mut pending_retention, &mut maintenance);
    loop {
        tokio::select! {
            maybe_command = receiver.recv() => {
                match maybe_command {
                    Some(LogDbCommand::Entry(entry)) => {
                        buffer.push(*entry);
                        if buffer.len() >= config.batch_size {
                            merge_pending_retention(
                                &state_db,
                                &mut pending_retention,
                                flush(&state_db, &mut buffer).await,
                            );
                        }
                    }
                    Some(LogDbCommand::Flush(reply)) => {
                        merge_pending_retention(
                            &state_db,
                            &mut pending_retention,
                            flush(&state_db, &mut buffer).await,
                        );
                        let _ = reply.send(());
                    }
                    None => {
                        merge_pending_retention(
                            &state_db,
                            &mut pending_retention,
                            flush(&state_db, &mut buffer).await,
                        );
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                retry_on_tick = false;
                merge_pending_retention(
                    &state_db,
                    &mut pending_retention,
                    flush(&state_db, &mut buffer).await,
                );
            }
            maintenance_result = async {
                if let Some(handle) = maintenance.as_mut() {
                    Some(handle.await)
                } else {
                    None
                }
            }, if maintenance.is_some() => {
                maintenance = None;
                let Some(maintenance_result) = maintenance_result else {
                    continue;
                };
                match maintenance_result {
                    Ok((_scope, Ok(()))) => {
                        state_db.record_log_retention_event("cleanup_completed");
                    }
                    Ok((scope, Err(_err))) => {
                        state_db.record_log_retention_event("cleanup_failed");
                        state_db.record_log_retention_event("cleanup_retry_pending");
                        merge_pending_retention(&state_db, &mut pending_retention, Some(scope));
                        retry_on_tick = true;
                    }
                    Err(_join_err) => {
                        state_db.record_log_retention_event("cleanup_failed");
                        state_db.record_log_retention_event("cleanup_retry_pending");
                        pending_retention = Some(LogRetentionScope::for_reconciliation());
                        retry_on_tick = true;
                    }
                }
            }
        }
        if !retry_on_tick {
            start_retention_maintenance(&state_db, &mut pending_retention, &mut maintenance);
        }
    }

    if let Some(handle) = maintenance.take() {
        match handle.await {
            Ok((_scope, Ok(()))) => state_db.record_log_retention_event("cleanup_completed"),
            Ok((scope, Err(_err))) => {
                state_db.record_log_retention_event("cleanup_failed");
                merge_pending_retention(&state_db, &mut pending_retention, Some(scope));
            }
            Err(_join_err) => {
                state_db.record_log_retention_event("cleanup_failed");
                pending_retention = Some(LogRetentionScope::for_reconciliation());
            }
        }
    }
    if let Some(scope) = pending_retention.take() {
        state_db.record_log_retention_event("cleanup_started");
        if state_db.prune_log_retention(scope).await.is_ok() {
            state_db.record_log_retention_event("cleanup_completed");
        } else {
            state_db.record_log_retention_event("cleanup_failed");
        }
    }
}

fn merge_pending_retention(
    state_db: &StateRuntime,
    pending: &mut Option<LogRetentionScope>,
    scope: Option<LogRetentionScope>,
) {
    let Some(scope) = scope else {
        return;
    };
    if let Some(pending) = pending.as_mut() {
        pending.merge(scope);
        state_db.record_log_retention_event("cleanup_coalesced");
    } else {
        *pending = Some(scope);
    }
}

fn start_retention_maintenance(
    state_db: &std::sync::Arc<StateRuntime>,
    pending: &mut Option<LogRetentionScope>,
    maintenance: &mut Option<tokio::task::JoinHandle<(LogRetentionScope, anyhow::Result<()>)>>,
) {
    if maintenance.is_some() {
        return;
    }
    let Some(scope) = pending.take() else {
        return;
    };
    state_db.record_log_retention_event("cleanup_started");
    let retry_scope = scope.clone();
    let state_db = std::sync::Arc::clone(state_db);
    *maintenance = Some(tokio::spawn(async move {
        let result = state_db.prune_log_retention(scope).await;
        (retry_scope, result)
    }));
}

async fn flush(state_db: &StateRuntime, buffer: &mut Vec<LogEntry>) -> Option<LogRetentionScope> {
    if buffer.is_empty() {
        return None;
    }
    let entries = buffer.split_off(0);
    state_db
        .insert_logs_deferred_retention(entries.as_slice())
        .await
        .ok()
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    thread_id: Option<String>,
}

impl MessageVisitor {
    fn record_field(&mut self, field: &Field, value: String) {
        if field.name() == "message" && self.message.is_none() {
            self.message = Some(value.clone());
        }
        if field.name() == "thread_id" && self.thread_id.is_none() {
            self.thread_id = Some(value);
        }
    }
}

impl Visit for MessageVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_field(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_field(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_field(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_field(field, value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_field(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_field(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_field(field, format!("{value:?}"));
    }
}

#[cfg(test)]
#[path = "log_db_filter_tests.rs"]
mod filter_tests;

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;
    use std::sync::Mutex;

    use pretty_assertions::assert_eq;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::fmt::writer::MakeWriter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    use super::*;

    #[derive(Default)]
    struct RetentionTelemetry {
        events: Mutex<Vec<String>>,
    }

    impl crate::DbTelemetry for RetentionTelemetry {
        fn counter(&self, name: &str, _inc: i64, tags: &[(&str, &str)]) {
            if name != crate::DB_LOG_RETENTION_METRIC {
                return;
            }
            if let Some(event) = tags
                .iter()
                .find_map(|(key, value)| (*key == "event").then(|| (*value).to_string()))
            {
                self.events.lock().expect("telemetry mutex").push(event);
            }
        }

        fn record_duration(
            &self,
            _name: &str,
            _duration: std::time::Duration,
            _tags: &[(&str, &str)],
        ) {
        }
    }

    impl RetentionTelemetry {
        fn event_count(&self, expected: &str) -> usize {
            self.events
                .lock()
                .expect("telemetry mutex")
                .iter()
                .filter(|event| event.as_str() == expected)
                .count()
        }
    }

    fn temp_codex_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("codex-state-log-db-{}", Uuid::new_v4()))
    }

    async fn wait_for_log_count(runtime: &StateRuntime, expected: usize) -> Vec<crate::LogRow> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let rows = runtime
                .query_logs(&crate::LogQuery::default())
                .await
                .expect("query logs");
            if rows.len() == expected {
                return rows;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {expected} logs; saw {}",
                rows.len()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn test_entry(message: &str) -> LogEntry {
        LogEntry {
            ts: 1,
            ts_nanos: 2,
            level: "INFO".to_string(),
            target: "test".to_string(),
            message: Some(message.to_string()),
            feedback_log_body: Some(message.to_string()),
            thread_id: Some("thread-1".to_string()),
            process_uuid: Some("process-1".to_string()),
            module_path: Some("module".to_string()),
            file: Some("file.rs".to_string()),
            line: Some(7),
        }
    }

    #[tokio::test]
    async fn retention_maintenance_coalesces_bursts_with_one_active_cleanup() {
        let codex_home = temp_codex_home();
        let telemetry = Arc::new(RetentionTelemetry::default());
        let runtime = StateRuntime::init_with_telemetry_for_tests(
            codex_home,
            "test-provider".to_string(),
            telemetry.clone(),
        )
        .await
        .expect("initialize runtime");
        let control = runtime.log_retention_test_control();
        control.block_next_deletion();

        let (sender, receiver) = mpsc::channel(32);
        let writer = tokio::spawn(run_inserter(
            Arc::clone(&runtime),
            receiver,
            LogSinkQueueConfig {
                queue_capacity: 32,
                batch_size: 1,
                flush_interval: std::time::Duration::from_millis(10),
            },
        ));
        control.wait_until_deletion_active().await;
        for index in 0..8 {
            sender
                .send(LogDbCommand::Entry(Box::new(test_entry(&format!(
                    "burst-{index}"
                )))))
                .await
                .expect("queue burst entry");
        }
        let (flush_sender, flush_receiver) = oneshot::channel();
        sender
            .send(LogDbCommand::Flush(flush_sender))
            .await
            .expect("queue flush");
        flush_receiver.await.expect("flush inserted burst");

        control.fail_next_deletion();
        control.release_blocked_deletion();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while telemetry.event_count("cleanup_completed") == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("failed cleanup retried on maintenance cadence");
        drop(sender);
        tokio::time::timeout(std::time::Duration::from_secs(2), writer)
            .await
            .expect("writer shutdown remains bounded")
            .expect("join writer");

        assert_eq!(control.max_active_deletions(), 1);
        assert!(telemetry.event_count("cleanup_coalesced") >= 1);
        assert_eq!(telemetry.event_count("cleanup_failed"), 1);
        assert_eq!(telemetry.event_count("cleanup_retry_pending"), 1);
    }

    #[tokio::test]
    async fn startup_reconciliation_prunes_retention_missed_before_shutdown() {
        let codex_home = temp_codex_home();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        drop(runtime);

        let logs_path = crate::logs_db_path(&codex_home);
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&logs_path)
                .create_if_missing(false),
        )
        .await
        .expect("open logs database");
        sqlx::query(
            "INSERT INTO logs (ts, ts_nanos, level, target, feedback_log_body, thread_id, process_uuid, estimated_bytes) VALUES (?, 0, 'INFO', 'recovery-test', 'oversized', 'recovery-thread', 'recovery-process', ?)",
        )
        .bind(chrono::Utc::now().timestamp())
        .bind(11_i64 * 1024 * 1024)
        .execute(&pool)
        .await
        .expect("seed missed retention row");
        pool.close().await;

        let recovered = StateRuntime::init(codex_home, "test-provider".to_string())
            .await
            .expect("reopen runtime");
        let (sender, receiver) = mpsc::channel(1);
        let writer = tokio::spawn(run_inserter(
            Arc::clone(&recovered),
            receiver,
            LogSinkQueueConfig {
                queue_capacity: 1,
                batch_size: 1,
                flush_interval: std::time::Duration::from_secs(60),
            },
        ));
        drop(sender);
        writer.await.expect("finish startup reconciliation");
        assert!(
            recovered
                .query_logs(&crate::LogQuery::default())
                .await
                .expect("query reconciled logs")
                .is_empty()
        );
    }

    #[derive(Clone, Default)]
    struct SharedWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl SharedWriter {
        fn snapshot(&self) -> String {
            String::from_utf8(self.bytes.lock().expect("writer mutex poisoned").clone())
                .expect("valid utf-8")
        }
    }

    struct SharedWriterGuard {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl<'a> MakeWriter<'a> for SharedWriter {
        type Writer = SharedWriterGuard;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriterGuard {
                bytes: Arc::clone(&self.bytes),
            }
        }
    }

    impl io::Write for SharedWriterGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes
                .lock()
                .expect("writer mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn sqlite_feedback_logs_match_feedback_formatter_shape() {
        let codex_home = temp_codex_home();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let writer = SharedWriter::default();
        let layer = start(runtime.clone());

        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer.clone())
                    .with_ansi(false)
                    .with_target(false)
                    .with_filter(Targets::new().with_default(tracing::Level::TRACE)),
            )
            .with(
                layer
                    .clone()
                    .with_filter(Targets::new().with_default(tracing::Level::TRACE)),
            );
        let guard = subscriber.set_default();

        tracing::trace!("threadless-before");
        tracing::info_span!("feedback-thread", thread_id = "thread-1", turn = 1).in_scope(|| {
            tracing::info!(foo = 2, "thread-scoped");
        });
        tracing::debug!("threadless-after");

        layer.flush().await;
        drop(guard);

        let feedback_logs = writer.snapshot();
        let without_timestamps = |logs: &str| {
            logs.lines()
                .map(|line| match line.split_once(' ') {
                    Some((_, rest)) => rest,
                    None => line,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let sqlite_logs = String::from_utf8(
            runtime
                .query_feedback_logs("thread-1")
                .await
                .expect("query feedback logs"),
        )
        .expect("valid utf-8");
        assert_eq!(
            without_timestamps(&sqlite_logs),
            without_timestamps(&feedback_logs)
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn flush_persists_logs_for_query() {
        let codex_home = temp_codex_home();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let layer = start(runtime.clone());

        let guard = tracing_subscriber::registry()
            .with(
                layer
                    .clone()
                    .with_filter(Targets::new().with_default(tracing::Level::TRACE)),
            )
            .set_default();

        tracing::info!("buffered-log");

        layer.flush().await;
        drop(guard);

        let after_flush = runtime
            .query_logs(&crate::LogQuery::default())
            .await
            .expect("query logs after flush");
        assert_eq!(after_flush.len(), 1);
        assert_eq!(after_flush[0].message.as_deref(), Some("buffered-log"));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn configured_batch_size_flushes_without_explicit_flush() {
        let codex_home = temp_codex_home();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let layer = LogDbLayer::start_with_config(
            runtime.clone(),
            LogSinkQueueConfig {
                queue_capacity: 8,
                batch_size: 2,
                flush_interval: std::time::Duration::from_secs(60),
            },
        );

        let guard = tracing_subscriber::registry()
            .with(
                layer
                    .clone()
                    .with_filter(Targets::new().with_default(tracing::Level::TRACE)),
            )
            .set_default();

        tracing::info!("first-batch-log");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(
            runtime
                .query_logs(&crate::LogQuery::default())
                .await
                .expect("query logs before batch fills")
                .len(),
            0
        );

        tracing::info!("second-batch-log");
        let after_batch = wait_for_log_count(&runtime, /*expected*/ 2).await;
        drop(guard);

        assert_eq!(
            after_batch
                .iter()
                .map(|row| row.message.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("first-batch-log"), Some("second-batch-log")]
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn configured_flush_interval_persists_buffered_logs() {
        let codex_home = temp_codex_home();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let layer = LogDbLayer::start_with_config(
            runtime.clone(),
            LogSinkQueueConfig {
                queue_capacity: 8,
                batch_size: 128,
                flush_interval: std::time::Duration::from_millis(10),
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let guard = tracing_subscriber::registry()
            .with(
                layer
                    .clone()
                    .with_filter(Targets::new().with_default(tracing::Level::TRACE)),
            )
            .set_default();

        tracing::info!("interval-log");
        let after_interval = wait_for_log_count(&runtime, /*expected*/ 1).await;
        drop(guard);

        assert_eq!(after_interval[0].message.as_deref(), Some("interval-log"));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn event_queue_drops_new_entries_when_full() {
        let (sender, mut receiver) = mpsc::channel(1);
        let layer = LogDbLayer {
            sender,
            process_uuid: "process-1".to_string(),
        };

        layer.try_send(test_entry("first-queued-log"));
        layer.try_send(test_entry("dropped-log"));

        match receiver.try_recv().expect("first entry queued") {
            LogDbCommand::Entry(entry) => {
                assert_eq!(entry.message.as_deref(), Some("first-queued-log"));
            }
            LogDbCommand::Flush(_) => panic!("expected queued entry"),
        }
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn flush_waits_for_queue_capacity_and_receiver_processing() {
        let (sender, mut receiver) = mpsc::channel(1);
        let layer = LogDbLayer {
            sender,
            process_uuid: "process-1".to_string(),
        };

        layer.try_send(test_entry("queued-before-flush"));
        let mut flush_task = tokio::spawn({
            let layer = layer.clone();
            async move {
                layer.flush().await;
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!flush_task.is_finished());

        match receiver.recv().await.expect("queued entry") {
            LogDbCommand::Entry(entry) => {
                assert_eq!(entry.message.as_deref(), Some("queued-before-flush"));
            }
            LogDbCommand::Flush(_) => panic!("expected queued entry"),
        }

        match receiver.recv().await.expect("flush command") {
            LogDbCommand::Flush(reply) => {
                assert!(!flush_task.is_finished());
                let _ = reply.send(());
            }
            LogDbCommand::Entry(_) => panic!("expected flush command"),
        }

        tokio::time::timeout(std::time::Duration::from_secs(1), &mut flush_task)
            .await
            .expect("flush task completes")
            .expect("flush task succeeds");
    }

    #[test]
    fn log_flushing_stays_on_the_concrete_layer() {
        let removed_trait = ["trait Log", "Writer"].concat();
        assert!(!include_str!("log_db.rs").contains(&removed_trait));
    }
}
