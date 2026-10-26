//! Tracing log export into the local SQLite log database.
//!
//! This module provides a `tracing_subscriber::Layer` that captures events,
//! formats each one into a `LogEntry`, and sends entries to a bounded background
//! queue. The background task inserts into the dedicated `logs` SQLite database
//! in batches to keep logging overhead low.
//!
//! ## Usage
//!
//! ```text
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
// Span records are diagnostic history, not an unbounded event log. Retain the
// newest UTF-8 suffix; thread identity is stored separately in SpanLogContext.
const MAX_SPAN_FIELDS_BYTES: usize = 16 * 1024;

pub fn default_filter() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_target("codex_http_client::transport", LevelFilter::OFF)
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

    /// Wait for queued inserts and report any insertion failure since the last flush.
    pub async fn flush(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(LogDbCommand::Flush(tx))
            .await
            .map_err(|_| anyhow::anyhow!("log inserter is closed"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("log inserter stopped before acknowledging flush"))?
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
                formatted_fields: bounded_span_fields(format_fields(attrs)),
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
                    formatted_fields: bounded_span_fields(format_fields(values)),
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

        let Ok(permit) = self.sender.try_reserve() else {
            return;
        };
        let mut visitor = SpanFieldVisitor::default();
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
            message: None,
            feedback_log_body: Some(feedback_log_body),
            thread_id,
            process_uuid: Some(self.process_uuid.clone()),
            module_path: metadata.module_path().map(ToString::to_string),
            file: metadata.file().map(ToString::to_string),
            line: metadata.line().map(|line| line as i64),
        };

        permit.send(LogDbCommand::Entry(Box::new(entry)));
    }
}

enum LogDbCommand {
    Entry(Box<LogEntry>),
    Flush(oneshot::Sender<anyhow::Result<()>>),
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
    fn record_field(&mut self, field: &Field, value: impl FnOnce() -> String) {
        if field.name() == "thread_id" && self.thread_id.is_none() {
            self.thread_id = Some(value());
        }
    }
}

impl Visit for SpanFieldVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_field(field, || value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_field(field, || value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_field(field, || value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_field(field, || value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_field(field, || value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_field(field, || value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_field(field, || format!("{value:?}"));
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
    *fields = bounded_span_fields(formatted.fields);
}

fn bounded_span_fields(fields: String) -> String {
    const MARKER: &str = "[earlier span fields truncated] ";
    if fields.len() > MAX_SPAN_FIELDS_BYTES {
        let mut start = fields.len() - (MAX_SPAN_FIELDS_BYTES - MARKER.len());
        while !fields.is_char_boundary(start) {
            start += 1;
        }
        // Allocate only the retained suffix: shortening in place would keep a
        // possibly huge original allocation alive for the lifetime of the span.
        return format!("{MARKER}{}", &fields[start..]);
    }
    fields
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
    let mut insert_error = None;
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
                                flush(&state_db, &mut buffer, &mut insert_error).await,
                            );
                        }
                    }
                    Some(LogDbCommand::Flush(reply)) => {
                        merge_pending_retention(
                            &state_db,
                            &mut pending_retention,
                            flush(&state_db, &mut buffer, &mut insert_error).await,
                        );
                        if let Err(Err(error)) = reply.send(insert_error.take().map_or(Ok(()), Err)) {
                            // Cancellation must not acknowledge an error that
                            // no live flush receiver was able to observe.
                            insert_error = Some(error);
                        }
                    }
                    None => {
                        merge_pending_retention(
                            &state_db,
                            &mut pending_retention,
                            flush(&state_db, &mut buffer, &mut insert_error).await,
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
                    flush(&state_db, &mut buffer, &mut insert_error).await,
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

async fn flush(
    state_db: &StateRuntime,
    buffer: &mut Vec<LogEntry>,
    insert_error: &mut Option<anyhow::Error>,
) -> Option<LogRetentionScope> {
    if buffer.is_empty() {
        return None;
    }
    let result = state_db
        .insert_logs_deferred_retention(buffer.as_slice())
        .await;
    // Logging remains best-effort; replay after an ambiguous commit could
    // duplicate entries. Retain capacity and surface failure to explicit flush.
    buffer.clear();
    match result {
        Ok(scope) => Some(scope),
        Err(error) => {
            if insert_error.is_none() {
                *insert_error = Some(error);
            }
            None
        }
    }
}


