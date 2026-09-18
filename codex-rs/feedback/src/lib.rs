use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::collections::btree_map::Entry;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use codex_login::AuthEnvTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use tracing::Event;
use tracing::Level;
use tracing::field::Visit;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::registry::LookupSpan;

pub(crate) mod feedback_diagnostics;
pub use feedback_diagnostics::FEEDBACK_DIAGNOSTICS_ATTACHMENT_FILENAME;
pub use feedback_diagnostics::FeedbackDiagnostic;
pub use feedback_diagnostics::FeedbackDiagnostics;

/// Filename used for the redacted `codex doctor --json` feedback attachment.
pub const DOCTOR_REPORT_ATTACHMENT_FILENAME: &str = "codex-doctor-report.json";
/// Filename used for the Windows sandbox log feedback attachment.
pub const WINDOWS_SANDBOX_LOG_ATTACHMENT_FILENAME: &str = "windows-sandbox.log";
const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
const SENTRY_DSN: &str =
    "https://ae32ed50620d7a7792c1ce5df38b3e3e@o33249.ingest.us.sentry.io/4510195390611458";
const UPLOAD_TIMEOUT_SECS: u64 = 10;
const FEEDBACK_TAGS_TARGET: &str = "feedback_tags";
const MAX_FEEDBACK_TAGS: usize = 64;
const MAX_FEEDBACK_TAG_VALUE_BYTES: usize = 1024;
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 40 * 1024 * 1024;

/// Structured request/auth fields that should be attached to feedback uploads.
pub struct FeedbackRequestTags<'a> {
    pub endpoint: &'a str,
    pub auth_header_attached: bool,
    pub auth_header_name: Option<&'a str>,
    pub auth_mode: Option<&'a str>,
    pub auth_retry_after_unauthorized: Option<bool>,
    pub auth_recovery_mode: Option<&'a str>,
    pub auth_recovery_phase: Option<&'a str>,
    pub auth_connection_reused: Option<bool>,
    pub auth_request_id: Option<&'a str>,
    pub auth_cf_ray: Option<&'a str>,
    pub auth_error: Option<&'a str>,
    pub auth_error_code: Option<&'a str>,
    pub auth_recovery_followup_success: Option<bool>,
    pub auth_recovery_followup_status: Option<u16>,
}

struct FeedbackRequestSnapshot<'a> {
    endpoint: &'a str,
    auth_header_attached: bool,
    auth_header_name: &'a str,
    auth_mode: &'a str,
    auth_retry_after_unauthorized: String,
    auth_recovery_mode: &'a str,
    auth_recovery_phase: &'a str,
    auth_connection_reused: String,
    auth_request_id: &'a str,
    auth_cf_ray: &'a str,
    auth_error: &'a str,
    auth_error_code: &'a str,
    auth_recovery_followup_success: String,
    auth_recovery_followup_status: String,
}

impl<'a> FeedbackRequestSnapshot<'a> {
    fn from_tags(tags: &'a FeedbackRequestTags<'a>) -> Self {
        Self {
            endpoint: tags.endpoint,
            auth_header_attached: tags.auth_header_attached,
            auth_header_name: tags.auth_header_name.unwrap_or(""),
            auth_mode: tags.auth_mode.unwrap_or(""),
            auth_retry_after_unauthorized: tags
                .auth_retry_after_unauthorized
                .map_or_else(String::new, |value| value.to_string()),
            auth_recovery_mode: tags.auth_recovery_mode.unwrap_or(""),
            auth_recovery_phase: tags.auth_recovery_phase.unwrap_or(""),
            auth_connection_reused: tags
                .auth_connection_reused
                .map_or_else(String::new, |value| value.to_string()),
            auth_request_id: tags.auth_request_id.unwrap_or(""),
            auth_cf_ray: tags.auth_cf_ray.unwrap_or(""),
            auth_error: tags.auth_error.unwrap_or(""),
            auth_error_code: tags.auth_error_code.unwrap_or(""),
            auth_recovery_followup_success: tags
                .auth_recovery_followup_success
                .map_or_else(String::new, |value| value.to_string()),
            auth_recovery_followup_status: tags
                .auth_recovery_followup_status
                .map_or_else(String::new, |value| value.to_string()),
        }
    }
}

pub fn emit_feedback_request_tags(tags: &FeedbackRequestTags<'_>) {
    let snapshot = FeedbackRequestSnapshot::from_tags(tags);
    tracing::info!(
        target: FEEDBACK_TAGS_TARGET,
        endpoint = snapshot.endpoint,
        auth_header_attached = snapshot.auth_header_attached,
        auth_header_name = snapshot.auth_header_name,
        auth_mode = snapshot.auth_mode,
        auth_retry_after_unauthorized = &snapshot.auth_retry_after_unauthorized,
        auth_recovery_mode = snapshot.auth_recovery_mode,
        auth_recovery_phase = snapshot.auth_recovery_phase,
        auth_connection_reused = &snapshot.auth_connection_reused,
        auth_request_id = snapshot.auth_request_id,
        auth_cf_ray = snapshot.auth_cf_ray,
        auth_error = snapshot.auth_error,
        auth_error_code = snapshot.auth_error_code,
        auth_recovery_followup_success = &snapshot.auth_recovery_followup_success,
        auth_recovery_followup_status = &snapshot.auth_recovery_followup_status,
    );
}

pub fn emit_feedback_request_tags_with_auth_env(
    tags: &FeedbackRequestTags<'_>,
    auth_env: &AuthEnvTelemetry,
) {
    let snapshot = FeedbackRequestSnapshot::from_tags(tags);
    tracing::info!(
        target: FEEDBACK_TAGS_TARGET,
        endpoint = snapshot.endpoint,
        auth_header_attached = snapshot.auth_header_attached,
        auth_header_name = snapshot.auth_header_name,
        auth_mode = snapshot.auth_mode,
        auth_retry_after_unauthorized = &snapshot.auth_retry_after_unauthorized,
        auth_recovery_mode = snapshot.auth_recovery_mode,
        auth_recovery_phase = snapshot.auth_recovery_phase,
        auth_connection_reused = &snapshot.auth_connection_reused,
        auth_request_id = snapshot.auth_request_id,
        auth_cf_ray = snapshot.auth_cf_ray,
        auth_error = snapshot.auth_error,
        auth_error_code = snapshot.auth_error_code,
        auth_recovery_followup_success = &snapshot.auth_recovery_followup_success,
        auth_recovery_followup_status = &snapshot.auth_recovery_followup_status,
        auth_env_openai_api_key_present = auth_env.openai_api_key_env_present,
        auth_env_codex_api_key_present = auth_env.codex_api_key_env_present,
        auth_env_codex_api_key_enabled = auth_env.codex_api_key_env_enabled,
        // Custom provider `env_key` is arbitrary config text, so emit only a safe bucket.
        auth_env_provider_key_name = auth_env.provider_env_key_name.as_deref().unwrap_or(""),
        auth_env_provider_key_present = auth_env.provider_env_key_present.map_or_else(String::new, |value| value.to_string()),
        auth_env_refresh_token_url_override_present = auth_env.refresh_token_url_override_present,
    );
}

#[derive(Clone)]
pub struct CodexFeedback {
    inner: Arc<FeedbackInner>,
}

impl Default for CodexFeedback {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexFeedback {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_BYTES)
    }

    pub(crate) fn with_capacity(max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(FeedbackInner::new(max_bytes)),
        }
    }

    pub fn make_writer(&self) -> FeedbackMakeWriter {
        FeedbackMakeWriter {
            inner: self.inner.clone(),
        }
    }

    /// Returns a [`tracing_subscriber`] layer that captures full-fidelity logs into this feedback
    /// ring buffer.
    ///
    /// This is intended for initialization code so call sites don't have to duplicate the exact
    /// `fmt::layer()` configuration and filter logic.
    pub fn logger_layer<S>(&self) -> impl Layer<S> + Send + Sync + 'static
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        tracing_subscriber::fmt::layer()
            .with_writer(self.make_writer())
            .with_timer(tracing_subscriber::fmt::time::SystemTime)
            .with_ansi(false)
            .with_target(false)
            // Capture everything, regardless of the caller's `RUST_LOG`, so feedback includes the
            // full trace when the user uploads a report.
            .with_filter(
                Targets::new()
                    .with_default(Level::TRACE)
                    .with_target("codex_core::post_sampling_token_estimate", LevelFilter::OFF),
            )
    }

    /// Returns a [`tracing_subscriber`] layer that collects structured metadata for feedback.
    ///
    /// Events with `target: "feedback_tags"` are treated as key/value tags to attach to feedback
    /// uploads later.
    pub fn metadata_layer<S>(&self) -> impl Layer<S> + Send + Sync + 'static
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        FeedbackMetadataLayer {
            inner: self.inner.clone(),
        }
        .with_filter(Targets::new().with_target(FEEDBACK_TAGS_TARGET, Level::TRACE))
    }

    pub fn snapshot(&self, session_id: Option<ThreadId>) -> FeedbackSnapshot {
        let bytes = {
            #[allow(clippy::expect_used)]
            let guard = self.inner.ring.lock().expect("mutex poisoned");
            guard.snapshot_bytes()
        };
        let tags = {
            #[allow(clippy::expect_used)]
            let guard = self.inner.tags.lock().expect("mutex poisoned");
            guard.clone()
        };
        FeedbackSnapshot {
            bytes,
            tags,
            feedback_diagnostics: FeedbackDiagnostics::collect_from_env(),
            thread_id: session_id
                .map(|id| id.to_string())
                .unwrap_or("no-active-thread-".to_string() + &ThreadId::new().to_string()),
        }
    }
}

struct FeedbackInner {
    ring: Mutex<RingBuffer>,
    tags: Mutex<BTreeMap<String, String>>,
}

impl FeedbackInner {
    fn new(max_bytes: usize) -> Self {
        Self {
            ring: Mutex::new(RingBuffer::new(max_bytes)),
            tags: Mutex::new(BTreeMap::new()),
        }
    }
}

#[derive(Clone)]
pub struct FeedbackMakeWriter {
    inner: Arc<FeedbackInner>,
}

impl<'a> MakeWriter<'a> for FeedbackMakeWriter {
    type Writer = FeedbackWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FeedbackWriter {
            inner: self.inner.clone(),
        }
    }
}

pub struct FeedbackWriter {
    inner: Arc<FeedbackInner>,
}

impl Write for FeedbackWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self.inner.ring.lock().map_err(|_| io::ErrorKind::Other)?;
        guard.push_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RingBuffer {
    max: usize,
    buf: VecDeque<u8>,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            max: capacity,
            buf: VecDeque::with_capacity(capacity),
        }
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn push_bytes(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        // If the incoming chunk is larger than capacity, keep only the trailing bytes.
        if data.len() >= self.max {
            self.buf.clear();
            let start = data.len() - self.max;
            self.buf.extend(data[start..].iter().copied());
            return;
        }

        // Evict from the front if we would exceed capacity.
        let needed = self.len() + data.len();
        if needed > self.max {
            let to_drop = needed - self.max;
            self.buf.drain(..to_drop);
        }

        self.buf.extend(data.iter().copied());
    }

    fn snapshot_bytes(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

pub struct FeedbackSnapshot {
    bytes: Vec<u8>,
    tags: BTreeMap<String, String>,
    feedback_diagnostics: FeedbackDiagnostics,
    pub thread_id: String,
}

pub struct FeedbackAttachmentPath {
    pub path: PathBuf,
    /// Optional filename to use for the uploaded attachment instead of `path`'s basename.
    pub attachment_filename_override: Option<String>,
}

/// In-memory attachment to include in a feedback upload.
///
/// Use this for generated diagnostics that should not be materialized on disk,
/// such as the redacted doctor report. File-backed artifacts should use
/// `FeedbackAttachmentPath` so upload-time read failures can be logged and
/// skipped independently.
pub struct FeedbackAttachment {
    /// Attachment filename shown in Sentry and in the feedback consent UI.
    pub filename: String,
    /// Optional MIME type for consumers that render or classify attachments.
    pub content_type: Option<String>,
    /// Attachment bytes captured before the upload starts.
    pub buffer: Vec<u8>,
}

/// Inputs that control one feedback upload to Sentry.
///
/// The caller is responsible for applying any user-consent gate before setting
/// `include_logs` or passing diagnostic attachments. This type only describes
/// what to upload once that decision has been made.
pub struct FeedbackUploadOptions<'a> {
    pub classification: &'a str,
    pub reason: Option<&'a str>,
    pub tags: Option<&'a BTreeMap<String, String>>,
    pub include_logs: bool,
    /// Generated attachments that are already buffered and safe to upload.
    ///
    /// These are included after `codex-logs.log` and before path-backed rollout
    /// attachments. They are only passed by the caller after any user consent
    /// gate has decided logs and diagnostics should be uploaded.
    pub extra_attachments: &'a [FeedbackAttachment],
    pub extra_attachment_paths: &'a [FeedbackAttachmentPath],
    pub session_source: Option<SessionSource>,
    pub logs_override: Option<Vec<u8>>,
}

impl FeedbackSnapshot {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn feedback_diagnostics(&self) -> &FeedbackDiagnostics {
        &self.feedback_diagnostics
    }

    pub fn with_feedback_diagnostics(mut self, feedback_diagnostics: FeedbackDiagnostics) -> Self {
        self.feedback_diagnostics = feedback_diagnostics;
        self
    }

    pub fn feedback_diagnostics_attachment_text(&self, include_logs: bool) -> Option<String> {
        if !include_logs {
            return None;
        }

        self.feedback_diagnostics.attachment_text()
    }

    /// Exports a unique snapshot. The caller owns removal of the returned file.
    pub fn save_to_temp_file(&self) -> io::Result<PathBuf> {
        let mut file = tempfile::Builder::new()
            .prefix("codex-feedback-")
            .suffix(".log")
            .tempfile()?;
        file.write_all(self.as_bytes())?;
        file.flush()?;
        let (_, path) = file.keep().map_err(|error| error.error)?;
        Ok(path)
    }

    /// Upload feedback to Sentry with optional attachments.
    pub fn upload_feedback(&self, options: FeedbackUploadOptions<'_>) -> Result<()> {
        use std::str::FromStr;
        use std::sync::Arc;

        use sentry::Client;
        use sentry::ClientOptions;
        use sentry::transports::DefaultTransportFactory;
        use sentry::types::Dsn;

        // Build Sentry client
        let client = Client::from_config(ClientOptions {
            dsn: Some(Dsn::from_str(SENTRY_DSN).map_err(|e| anyhow!("invalid DSN: {e}"))?),
            transport: Some(Arc::new(DefaultTransportFactory {})),
            ..Default::default()
        });

        self.upload_feedback_with_client(options, &client)
    }

    fn upload_feedback_with_client(
        &self,
        options: FeedbackUploadOptions<'_>,
        client: &sentry::Client,
    ) -> Result<()> {
        use sentry::protocol::Envelope;
        use sentry::protocol::EnvelopeItem;
        use sentry::protocol::Event;
        use sentry::protocol::Level;

        let tags = self.upload_tags(
            options.classification,
            options.reason,
            options.tags,
            options.session_source.as_ref(),
        );

        let level = match options.classification {
            "bug" | "bad_result" | "safety_check" => Level::Error,
            _ => Level::Info,
        };

        let mut envelope = Envelope::new();
        let title = format!(
            "[{}]: Codex session {}",
            display_classification(options.classification),
            self.thread_id
        );

        let mut event = Event {
            level,
            message: Some(title.clone()),
            tags,
            ..Default::default()
        };
        if let Some(r) = options.reason {
            use sentry::protocol::Exception;
            use sentry::protocol::Values;

            event.exception = Values::from(vec![Exception {
                ty: title,
                value: Some(r.to_string()),
                ..Default::default()
            }]);
        }
        envelope.add_item(EnvelopeItem::Event(event));

        for attachment in self.feedback_attachments(
            options.include_logs,
            options.extra_attachments,
            options.extra_attachment_paths,
            options.logs_override,
        ) {
            envelope.add_item(EnvelopeItem::Attachment(attachment));
        }

        client.send_envelope(envelope);
        if !client.flush(Some(Duration::from_secs(UPLOAD_TIMEOUT_SECS))) {
            return Err(anyhow!("feedback upload did not finish before the timeout"));
        }
        Ok(())
    }

    fn upload_tags(
        &self,
        classification: &str,
        reason: Option<&str>,
        client_tags: Option<&BTreeMap<String, String>>,
        session_source: Option<&SessionSource>,
    ) -> BTreeMap<String, String> {
        let cli_version = env!("CARGO_PKG_VERSION");
        let mut tags = BTreeMap::from([
            (String::from("thread_id"), self.thread_id.to_string()),
            (String::from("classification"), classification.to_string()),
            (String::from("cli_version"), cli_version.to_string()),
        ]);
        if let Some(source) = session_source {
            tags.insert(String::from("session_source"), source.to_string());
        }
        if let Some(r) = reason {
            tags.insert(String::from("reason"), r.to_string());
        }

        let reserved = [
            "thread_id",
            "classification",
            "cli_version",
            "session_source",
            "reason",
        ];
        if let Some(client_tags) = client_tags {
            for (key, value) in client_tags {
                if reserved.contains(&key.as_str()) {
                    continue;
                }
                if let Entry::Vacant(entry) = tags.entry(key.clone()) {
                    entry.insert(value.clone());
                }
            }
        }
        for (key, value) in &self.tags {
            if reserved.contains(&key.as_str()) {
                continue;
            }
            if let Entry::Vacant(entry) = tags.entry(key.clone()) {
                entry.insert(value.clone());
            }
        }

        tags
    }

    fn feedback_attachments(
        &self,
        include_logs: bool,
        extra_attachments: &[FeedbackAttachment],
        extra_attachment_paths: &[FeedbackAttachmentPath],
        logs_override: Option<Vec<u8>>,
    ) -> Vec<sentry::protocol::Attachment> {
        use sentry::protocol::Attachment;

        let mut attachments = Vec::new();
        let mut remaining = MAX_TOTAL_ATTACHMENT_BYTES;

        if include_logs {
            let logs = logs_override.as_deref().unwrap_or(&self.bytes);
            if reserve_attachment_bytes("codex-logs.log", logs.len(), &mut remaining) {
                attachments.push(Attachment {
                    buffer: logs_override.unwrap_or_else(|| self.bytes.clone()),
                    filename: String::from("codex-logs.log"),
                    content_type: Some("text/plain".to_string()),
                    ty: None,
                });
            }
        }

        attachments.extend(
            extra_attachments
                .iter()
                .filter(|attachment| {
                    reserve_attachment_bytes(
                        &attachment.filename,
                        attachment.buffer.len(),
                        &mut remaining,
                    )
                })
                .map(|attachment| Attachment {
                    buffer: attachment.buffer.clone(),
                    filename: attachment.filename.clone(),
                    content_type: attachment.content_type.clone(),
                    ty: None,
                }),
        );

        if let Some(text) = self.feedback_diagnostics_attachment_text(include_logs)
            && reserve_attachment_bytes(
                FEEDBACK_DIAGNOSTICS_ATTACHMENT_FILENAME,
                text.len(),
                &mut remaining,
            )
        {
            attachments.push(Attachment {
                buffer: text.into_bytes(),
                filename: FEEDBACK_DIAGNOSTICS_ATTACHMENT_FILENAME.to_string(),
                content_type: Some("text/plain".to_string()),
                ty: None,
            });
        }

        for attachment_path in extra_attachment_paths {
            let limit = remaining.min(MAX_ATTACHMENT_BYTES);
            let data = match fs::File::open(&attachment_path.path).and_then(|file| {
                let mut data = Vec::new();
                file.take(limit as u64 + 1).read_to_end(&mut data)?;
                Ok(data)
            }) {
                Ok(data) => data,
                Err(err) => {
                    tracing::warn!(
                        path = %attachment_path.path.display(),
                        error = %err,
                        "failed to read log attachment; skipping"
                    );
                    continue;
                }
            };
            let filename = attachment_path
                .attachment_filename_override
                .clone()
                .unwrap_or_else(|| {
                    attachment_path
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "extra-log.log".to_string())
                });
            if !reserve_attachment_bytes(&filename, data.len(), &mut remaining) {
                continue;
            }
            let content_type = match Path::new(&filename)
                .extension()
                .and_then(|extension| extension.to_str())
            {
                Some(extension) if extension.eq_ignore_ascii_case("jsonl") => {
                    "text/plain".to_string()
                }
                _ => mime_guess::from_path(&filename)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string(),
            };
            attachments.push(Attachment {
                buffer: data,
                filename,
                content_type: Some(content_type),
                ty: None,
            });
        }

        attachments
    }
}

fn reserve_attachment_bytes(filename: &str, bytes: usize, remaining: &mut usize) -> bool {
    if bytes > MAX_ATTACHMENT_BYTES || bytes > *remaining {
        tracing::warn!(
            filename,
            bytes,
            "feedback attachment exceeds upload byte budget; skipping"
        );
        return false;
    }
    *remaining -= bytes;
    true
}

fn display_classification(classification: &str) -> String {
    match classification {
        "bug" => "Bug".to_string(),
        "bad_result" => "Bad result".to_string(),
        "good_result" => "Good result".to_string(),
        "safety_check" => "Safety check".to_string(),
        _ => "Other".to_string(),
    }
}

#[derive(Clone)]
struct FeedbackMetadataLayer {
    inner: Arc<FeedbackInner>,
}

impl<S> Layer<S> for FeedbackMetadataLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        // This layer is filtered by `Targets`, but keep the guard anyway in case it is used without
        // the filter.
        if event.metadata().target() != FEEDBACK_TAGS_TARGET {
            return;
        }

        let mut visitor = FeedbackTagsVisitor::default();
        event.record(&mut visitor);
        if visitor.tags.is_empty() {
            return;
        }

        #[allow(clippy::expect_used)]
        let mut guard = self.inner.tags.lock().expect("mutex poisoned");
        for (key, value) in visitor.tags {
            if guard.len() >= MAX_FEEDBACK_TAGS && !guard.contains_key(&key) {
                continue;
            }
            guard.insert(key, value);
        }
    }
}

#[derive(Default)]
struct FeedbackTagsVisitor {
    tags: BTreeMap<String, String>,
}

impl Visit for FeedbackTagsVisitor {
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.tags.insert(
            field.name().to_string(),
            value[..value.floor_char_boundary(MAX_FEEDBACK_TAG_VALUE_BYTES)].to_string(),
        );
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let mut formatted = BoundedTagValue(String::new());
        let _ = write!(formatted, "{value:?}");
        self.tags.insert(field.name().to_string(), formatted.0);
    }
}

struct BoundedTagValue(String);

impl std::fmt::Write for BoundedTagValue {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let available = MAX_FEEDBACK_TAG_VALUE_BYTES - self.0.len();
        let end = value.floor_char_boundary(available);
        self.0.push_str(&value[..end]);
        if end < value.len() {
            Err(std::fmt::Error)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;

    use super::*;
    use crate::FeedbackDiagnostic;
    use pretty_assertions::assert_eq;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    #[test]
    fn upload_reports_transport_flush_failure() {
        struct TestTransport {
            flushed: bool,
            envelopes: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl sentry::Transport for TestTransport {
            fn send_envelope(&self, envelope: sentry::protocol::Envelope) {
                let classification = envelope.items().find_map(|item| match item {
                    sentry::protocol::EnvelopeItem::Event(event) => {
                        event.tags.get("classification").map(String::as_str)
                    }
                    _ => None,
                });
                assert_eq!(classification, Some("bug"));
                self.envelopes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            fn flush(&self, _: Duration) -> bool {
                self.flushed
            }
        }
        for flushed in [false, true] {
            let envelopes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let transport: Arc<dyn sentry::Transport> = Arc::new(TestTransport {
                flushed,
                envelopes: envelopes.clone(),
            });
            let client = sentry::Client::from_config(sentry::ClientOptions {
                dsn: Some(SENTRY_DSN.parse().unwrap()),
                transport: Some(Arc::new(move |_: &sentry::ClientOptions| transport.clone())),
                ..Default::default()
            });
            let snapshot = CodexFeedback::new().snapshot(None);
            let result = snapshot.upload_feedback_with_client(
                FeedbackUploadOptions {
                    classification: "bug",
                    reason: None,
                    tags: None,
                    include_logs: false,
                    extra_attachments: &[],
                    extra_attachment_paths: &[],
                    session_source: None,
                    logs_override: None,
                },
                &client,
            );
            assert_eq!(envelopes.load(std::sync::atomic::Ordering::SeqCst), 1);
            if flushed {
                result.unwrap();
            } else {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "feedback upload did not finish before the timeout"
                );
            }
        }
    }

    #[test]
    fn temporary_exports_preserve_distinct_snapshots() {
        let fb = CodexFeedback::new();
        let thread_id = ThreadId::new();
        let mut writer = fb.make_writer().make_writer();
        writer.write_all(b"first").unwrap();
        let first = fb.snapshot(Some(thread_id)).save_to_temp_file().unwrap();
        writer.write_all(b" second").unwrap();
        let second = fb.snapshot(Some(thread_id)).save_to_temp_file().unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"first");
        assert_eq!(fs::read(&second).unwrap(), b"first second");
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
    }

    #[test]
    fn attachment_budgets_skip_oversized_inputs_without_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = dir.path().join("large.zip");
        fs::File::create(&oversized)
            .unwrap()
            .set_len((MAX_ATTACHMENT_BYTES + 1) as u64)
            .unwrap();
        let small = dir.path().join("small.zip");
        fs::write(&small, b"complete archive").unwrap();
        let paths = [oversized, small].map(|path| FeedbackAttachmentPath {
            path,
            attachment_filename_override: None,
        });
        let snapshot = CodexFeedback::new().snapshot(None);
        let attachments = snapshot.feedback_attachments(false, &[], &paths, None);
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "small.zip");
        assert_eq!(attachments[0].buffer, b"complete archive");

        let extras = [
            MAX_ATTACHMENT_BYTES + 1,
            MAX_ATTACHMENT_BYTES,
            MAX_ATTACHMENT_BYTES,
            1,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, size)| FeedbackAttachment {
            filename: format!("{index}.zip"),
            content_type: None,
            buffer: vec![index as u8; size],
        })
        .collect::<Vec<_>>();
        let attachments = snapshot.feedback_attachments(false, &extras, &paths, None);
        assert_eq!(
            attachments
                .iter()
                .map(|item| item.filename.as_str())
                .collect::<Vec<_>>(),
            ["1.zip", "2.zip"]
        );
        assert_eq!(
            attachments
                .iter()
                .map(|item| item.buffer.len())
                .sum::<usize>(),
            MAX_TOTAL_ATTACHMENT_BYTES
        );
        assert!(attachments[0].buffer.iter().all(|byte| *byte == 1));
        assert!(attachments[1].buffer.iter().all(|byte| *byte == 2));
    }

    #[test]
    fn request_emitters_preserve_plain_values_and_clear_absent_tags() {
        let fb = CodexFeedback::new();
        let _guard = tracing_subscriber::registry()
            .with(fb.metadata_layer())
            .set_default();
        let mut tags = FeedbackRequestTags {
            endpoint: "https://example.test/\"quoted\"",
            auth_header_attached: true,
            auth_header_name: Some("Authorization"),
            auth_mode: Some("chatgpt"),
            auth_retry_after_unauthorized: Some(false),
            auth_recovery_mode: None,
            auth_recovery_phase: None,
            auth_connection_reused: Some(true),
            auth_request_id: Some("request-id"),
            auth_cf_ray: None,
            auth_error: None,
            auth_error_code: None,
            auth_recovery_followup_success: Some(true),
            auth_recovery_followup_status: Some(200),
        };
        emit_feedback_request_tags(&tags);
        let snapshot = fb.snapshot(None);
        assert_eq!(snapshot.tags["endpoint"], tags.endpoint);
        assert_eq!(snapshot.tags["auth_header_name"], "Authorization");
        assert_eq!(snapshot.tags["auth_retry_after_unauthorized"], "false");
        assert_eq!(snapshot.tags["auth_recovery_followup_status"], "200");
        assert_eq!(snapshot.tags["auth_request_id"], "request-id");
        tags.auth_request_id = None;
        tags.auth_retry_after_unauthorized = None;
        emit_feedback_request_tags_with_auth_env(&tags, &AuthEnvTelemetry::default());
        let snapshot = fb.snapshot(None);
        assert_eq!(snapshot.tags["endpoint"], tags.endpoint);
        assert_eq!(snapshot.tags["auth_request_id"], "");
        assert_eq!(snapshot.tags["auth_retry_after_unauthorized"], "");
        assert_eq!(snapshot.tags["auth_env_openai_api_key_present"], "false");
    }

    #[test]
    fn metadata_values_are_bounded_during_collection_and_formatting() {
        struct LargeDebug;
        impl std::fmt::Debug for LargeDebug {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                for _ in 0..MAX_FEEDBACK_TAG_VALUE_BYTES + 1 {
                    formatter.write_str("x")?;
                }
                panic!("formatting must stop when the byte budget is exhausted");
            }
        }
        let fb = CodexFeedback::new();
        let _guard = tracing_subscriber::registry()
            .with(fb.metadata_layer())
            .set_default();
        let value = "€".repeat(MAX_FEEDBACK_TAG_VALUE_BYTES);
        tracing::info!(target: FEEDBACK_TAGS_TARGET, text = value.as_str(), debug = ?LargeDebug);
        let snapshot = fb.snapshot(None);
        assert_eq!(
            snapshot.tags["text"],
            "€".repeat(MAX_FEEDBACK_TAG_VALUE_BYTES / 3)
        );
        assert_eq!(
            snapshot.tags["debug"],
            "x".repeat(MAX_FEEDBACK_TAG_VALUE_BYTES)
        );
        {
            let mut tags = fb.inner.tags.lock().unwrap();
            for index in 0..MAX_FEEDBACK_TAGS - 2 {
                tags.insert(format!("filler-{index}"), String::new());
            }
        }
        tracing::info!(target: FEEDBACK_TAGS_TARGET, text = "updated", rejected = "new key");
        let snapshot = fb.snapshot(None);
        assert_eq!(snapshot.tags.len(), MAX_FEEDBACK_TAGS);
        assert_eq!(snapshot.tags["text"], "updated");
        assert!(!snapshot.tags.contains_key("rejected"));
    }

    #[test]
    fn ring_buffer_drops_front_when_full() {
        let fb = CodexFeedback::with_capacity(/*max_bytes*/ 8);
        {
            let mut w = fb.make_writer().make_writer();
            w.write_all(b"abcdefgh").unwrap();
            w.write_all(b"ij").unwrap();
        }
        let snap = fb.snapshot(/*session_id*/ None);
        // Capacity 8: after writing 10 bytes, we should keep the last 8.
        pretty_assertions::assert_eq!(std::str::from_utf8(snap.as_bytes()).unwrap(), "cdefghij");
    }

    #[test]
    fn configured_sentry_transport_delivers_envelope() {
        use std::io::Read;
        use std::net::TcpListener;
        use std::time::Instant;

        use sentry::TransportFactory;
        use sentry::protocol::Envelope;
        use sentry::protocol::EnvelopeItem;
        use sentry::transports::DefaultTransportFactory;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local Sentry endpoint");
        let address = listener.local_addr().expect("local endpoint address");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "transport sent no request");
                        std::thread::sleep(
                            Duration::from_millis(10)
                                .min(deadline.saturating_duration_since(Instant::now())),
                        );
                    }
                    Err(error) => panic!("accept local request: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("blocking accepted socket");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("bound request read");
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = stream.read(&mut chunk).expect("read envelope request");
                assert!(read > 0, "request ended before complete envelope");
                request.extend_from_slice(&chunk[..read]);
                assert!(request.len() <= 64 * 1024, "unexpected request size");
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let body_start = header_end + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let body_len: usize = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .expect("envelope content length")
                        .1
                        .trim()
                        .parse()
                        .expect("numeric content length");
                    if request.len() >= body_start + body_len {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                            .expect("acknowledge envelope");
                        return request;
                    }
                }
            }
        });
        let options = sentry::ClientOptions {
            dsn: Some(
                format!("http://public@{address}/1")
                    .parse()
                    .expect("test DSN should parse"),
            ),
            ..Default::default()
        };

        let transport = DefaultTransportFactory.create_transport(&options);
        let mut envelope = Envelope::new();
        envelope.add_item(EnvelopeItem::Event(sentry::protocol::Event {
            message: Some("configured-feedback-transport-marker".to_string()),
            ..Default::default()
        }));
        transport.send_envelope(envelope);
        assert!(transport.flush(Duration::from_secs(10)));
        let request = server.join().expect("local endpoint completed");
        let request = String::from_utf8(request).expect("UTF-8 envelope request");
        assert!(
            request.starts_with("POST /api/1/envelope/ HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains("configured-feedback-transport-marker"),
            "{request}"
        );
        assert!(
            request.to_ascii_lowercase().contains("x-sentry-auth:"),
            "{request}"
        );
    }

    #[test]
    fn metadata_layer_records_tags_from_feedback_target() {
        let fb = CodexFeedback::new();
        let _guard = tracing_subscriber::registry()
            .with(fb.metadata_layer())
            .set_default();

        tracing::info!(target: FEEDBACK_TAGS_TARGET, model = "gpt-5", cached = true, "tags");

        let snap = fb.snapshot(/*session_id*/ None);
        pretty_assertions::assert_eq!(snap.tags.get("model").map(String::as_str), Some("gpt-5"));
        pretty_assertions::assert_eq!(snap.tags.get("cached").map(String::as_str), Some("true"));
    }

    #[test]
    fn feedback_attachments_gate_connectivity_diagnostics() {
        let extra_filename = format!("codex-feedback-extra-{}.jsonl", ThreadId::new());
        let extra_path = std::env::temp_dir().join(&extra_filename);
        let extra_attachment_path = FeedbackAttachmentPath {
            path: extra_path.clone(),
            attachment_filename_override: None,
        };
        fs::write(&extra_path, "rollout").expect("extra attachment should be written");

        let snapshot_with_diagnostics = CodexFeedback::new()
            .snapshot(/*session_id*/ None)
            .with_feedback_diagnostics(FeedbackDiagnostics::new(vec![FeedbackDiagnostic {
                headline: "Proxy environment variables are set and may affect connectivity."
                    .to_string(),
                details: vec!["HTTPS_PROXY = https://example.com:443".to_string()],
            }]));

        let attachments_with_diagnostics = snapshot_with_diagnostics.feedback_attachments(
            /*include_logs*/ true,
            &[FeedbackAttachment {
                filename: DOCTOR_REPORT_ATTACHMENT_FILENAME.to_string(),
                content_type: Some("application/json".to_string()),
                buffer: b"{\"overallStatus\":\"ok\"}".to_vec(),
            }],
            std::slice::from_ref(&extra_attachment_path),
            Some(vec![1]),
        );

        assert_eq!(
            attachments_with_diagnostics
                .iter()
                .map(|attachment| attachment.filename.as_str())
                .collect::<Vec<_>>(),
            vec![
                "codex-logs.log",
                DOCTOR_REPORT_ATTACHMENT_FILENAME,
                FEEDBACK_DIAGNOSTICS_ATTACHMENT_FILENAME,
                extra_filename.as_str()
            ]
        );
        assert_eq!(attachments_with_diagnostics[0].buffer, vec![1]);
        assert_eq!(
            attachments_with_diagnostics[1].buffer,
            b"{\"overallStatus\":\"ok\"}".to_vec()
        );
        assert_eq!(
            attachments_with_diagnostics[2].buffer,
            b"Connectivity diagnostics\n\n- Proxy environment variables are set and may affect connectivity.\n  - HTTPS_PROXY = https://example.com:443".to_vec()
        );
        assert_eq!(attachments_with_diagnostics[3].buffer, b"rollout".to_vec());
        assert_eq!(
            attachments_with_diagnostics[3].content_type.as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            OsStr::new(attachments_with_diagnostics[3].filename.as_str()),
            OsStr::new(extra_filename.as_str())
        );
        let attachments_without_diagnostics = CodexFeedback::new()
            .snapshot(/*session_id*/ None)
            .with_feedback_diagnostics(FeedbackDiagnostics::default())
            .feedback_attachments(/*include_logs*/ true, &[], &[], Some(vec![1]));

        assert_eq!(
            attachments_without_diagnostics
                .iter()
                .map(|attachment| attachment.filename.as_str())
                .collect::<Vec<_>>(),
            vec!["codex-logs.log"]
        );
        assert_eq!(attachments_without_diagnostics[0].buffer, vec![1]);
        fs::remove_file(extra_path).expect("extra attachment should be removed");
    }

    #[test]
    fn path_backed_attachments_use_binary_content_types() {
        let suffix = ThreadId::new();
        let gzip_filename = format!("codex-desktop-app-logs-{suffix}.tar.gz");
        let unknown_filename = format!("codex-feedback-extra-{suffix}.binunknown");
        let gzip_path = std::env::temp_dir().join(&gzip_filename);
        let unknown_path = std::env::temp_dir().join(&unknown_filename);
        let gzip_bytes = b"\x1f\x8b\x08\x00\xff";
        let unknown_bytes = b"\x00\x9f\x92\x96";
        fs::write(&gzip_path, gzip_bytes).expect("gzip attachment should be written");
        fs::write(&unknown_path, unknown_bytes).expect("unknown attachment should be written");

        let attachments = CodexFeedback::new()
            .snapshot(/*session_id*/ None)
            .feedback_attachments(
                /*include_logs*/ false,
                &[],
                &[
                    FeedbackAttachmentPath {
                        path: gzip_path.clone(),
                        attachment_filename_override: None,
                    },
                    FeedbackAttachmentPath {
                        path: unknown_path.clone(),
                        attachment_filename_override: None,
                    },
                ],
                /*logs_override*/ None,
            );

        fs::remove_file(gzip_path).expect("gzip attachment should be removed");
        fs::remove_file(unknown_path).expect("unknown attachment should be removed");
        assert_eq!(
            attachments
                .iter()
                .map(|attachment| (
                    attachment.filename.as_str(),
                    attachment.content_type.as_deref(),
                    attachment.buffer.as_slice(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    gzip_filename.as_str(),
                    Some("application/gzip"),
                    gzip_bytes.as_slice(),
                ),
                (
                    unknown_filename.as_str(),
                    Some("application/octet-stream"),
                    unknown_bytes.as_slice(),
                ),
            ]
        );
    }

    #[test]
    fn upload_tags_include_client_tags_and_preserve_reserved_fields() {
        let mut tags = BTreeMap::new();
        tags.insert("thread_id".to_string(), "wrong-thread".to_string());
        tags.insert("turn_id".to_string(), "wrong-turn".to_string());
        tags.insert(
            "classification".to_string(),
            "wrong-classification".to_string(),
        );
        tags.insert("cli_version".to_string(), "wrong-version".to_string());
        tags.insert("session_source".to_string(), "wrong-source".to_string());
        tags.insert("reason".to_string(), "wrong-reason".to_string());
        tags.insert("account_id".to_string(), "actual-account".to_string());
        tags.insert("model".to_string(), "gpt-5".to_string());
        let snapshot = FeedbackSnapshot {
            bytes: Vec::new(),
            tags,
            feedback_diagnostics: FeedbackDiagnostics::default(),
            thread_id: "thread-123".to_string(),
        };
        let mut client_tags = BTreeMap::new();
        client_tags.insert("thread_id".to_string(), "wrong-client-thread".to_string());
        client_tags.insert("turn_id".to_string(), "turn-456".to_string());
        client_tags.insert(
            "classification".to_string(),
            "wrong-client-classification".to_string(),
        );
        client_tags.insert(
            "cli_version".to_string(),
            "wrong-client-version".to_string(),
        );
        client_tags.insert(
            "session_source".to_string(),
            "wrong-client-source".to_string(),
        );
        client_tags.insert("reason".to_string(), "wrong-client-reason".to_string());
        client_tags.insert("client_tag".to_string(), "from-client".to_string());

        let upload_tags = snapshot.upload_tags(
            "bug",
            Some("actual reason"),
            Some(&client_tags),
            Some(&SessionSource::Cli),
        );

        assert_eq!(
            upload_tags.get("thread_id").map(String::as_str),
            Some("thread-123")
        );
        assert_eq!(
            upload_tags.get("turn_id").map(String::as_str),
            Some("turn-456")
        );
        assert_eq!(
            upload_tags.get("classification").map(String::as_str),
            Some("bug")
        );
        assert_eq!(
            upload_tags.get("session_source").map(String::as_str),
            Some("cli")
        );
        assert_eq!(
            upload_tags.get("reason").map(String::as_str),
            Some("actual reason")
        );
        assert_eq!(
            upload_tags.get("account_id").map(String::as_str),
            Some("actual-account")
        );
        assert_eq!(
            upload_tags.get("client_tag").map(String::as_str),
            Some("from-client")
        );
        assert_eq!(upload_tags.get("model").map(String::as_str), Some("gpt-5"));
        assert_eq!(
            upload_tags.get("cli_version").map(String::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }
}
