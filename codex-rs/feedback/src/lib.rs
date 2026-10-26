use std::borrow::Cow;
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
use codex_http_client::BlockingHttpClient;
use codex_http_client::BlockingHttpClientBuilder;
use codex_login::AuthEnvTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use sentry::types::Dsn;
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
/// Allowance for connecting and for Sentry to acknowledge the envelope.
const UPLOAD_BASE_TIMEOUT: Duration = Duration::from_secs(10);
/// Conservative sustained upload rate that extends the deadline for large envelopes.
const UPLOAD_MIN_BYTES_PER_SEC: u64 = 256 * 1024;
/// Envelope headers and the event item, beyond the attachment payloads.
const ENVELOPE_OVERHEAD_BYTES: usize = 64 * 1024;
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
            // full trace when the user uploads a report. OTel export-only targets stay with their
            // exporters, as in the SQLite log sink: accepting them here would keep telemetry
            // builders enabled without an exporter and flood the buffer with per-event records.
            .with_filter(
                Targets::new()
                    .with_default(Level::TRACE)
                    .with_target("codex_http_client::transport", LevelFilter::OFF)
                    .with_target("codex_core::post_sampling_token_estimate", LevelFilter::OFF)
                    .with_target("codex_otel.log_only", LevelFilter::OFF)
                    .with_target("codex_otel.trace_safe", LevelFilter::OFF),
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
                .unwrap_or_else(|| format!("no-active-thread-{}", ThreadId::new())),
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
    dropped_bytes: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            max: capacity,
            buf: VecDeque::with_capacity(capacity),
            dropped_bytes: 0,
        }
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn push_bytes(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let dropped = data.len().saturating_sub(self.max - self.len());
        self.dropped_bytes = self.dropped_bytes.saturating_add(dropped);

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
        if self.dropped_bytes == 0 {
            return self.buf.iter().copied().collect();
        }
        // Add the marker only at capture time so it cannot evict useful log bytes.
        let mut bytes = format!(
            "[codex-feedback: log truncated; {} earlier bytes discarded]\n",
            self.dropped_bytes
        )
        .into_bytes();
        bytes.reserve(self.buf.len());
        bytes.extend(self.buf.iter().copied());
        bytes
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
    /// A selected rollout, rather than an arbitrary user-supplied log. Hydrate
    /// validated sidecars within the upload budget instead of exporting stubs.
    pub hydrate_rollout: bool,
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

struct FeedbackUploadAttachment<'a> {
    buffer: Cow<'a, [u8]>,
    filename: Cow<'a, str>,
    content_type: Option<Cow<'a, str>>,
}

impl FeedbackUploadAttachment<'_> {
    fn to_writer(&self, mut writer: impl Write) -> io::Result<()> {
        use sentry::protocol::Value;

        // Match Sentry's attachment headers without requiring an owned payload copy.
        writeln!(
            writer,
            r#"{{"type":"attachment","length":{},"filename":{},"attachment_type":"event.attachment","content_type":{}}}"#,
            self.buffer.len(),
            Value::from(self.filename.as_ref()),
            Value::from(
                self.content_type
                    .as_deref()
                    .unwrap_or("application/octet-stream")
            ),
        )?;
        writer.write_all(self.buffer.as_ref())?;
        writeln!(writer)
    }
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

    /// Uploads feedback to Sentry and returns once Sentry has accepted it.
    ///
    /// Blocks the calling thread until the envelope is accepted, rejected, or the size-scaled
    /// upload deadline expires; async callers must run it on a blocking thread. An error means
    /// the report was not accepted.
    pub fn upload_feedback(&self, options: FeedbackUploadOptions<'_>) -> Result<()> {
        let dsn: Dsn = SENTRY_DSN
            .parse()
            .map_err(|e| anyhow!("invalid DSN: {e}"))?;
        let client = BlockingHttpClientBuilder::new()
            .build_with_transport_default_proxy()
            .map_err(|err| anyhow!("failed to build feedback upload client: {err}"))?;
        self.upload_feedback_with_client(options, &client, &dsn)
    }

    fn upload_feedback_with_client(
        &self,
        options: FeedbackUploadOptions<'_>,
        client: &BlockingHttpClient,
        dsn: &Dsn,
    ) -> Result<()> {
        let body = self.encode_envelope(options)?;
        let deadline = upload_deadline(body.len());
        send_envelope(client, dsn, body, deadline)
    }

    fn encode_envelope(&self, options: FeedbackUploadOptions<'_>) -> Result<Vec<u8>> {
        use sentry::protocol::Envelope;
        use sentry::protocol::EnvelopeItem;
        use sentry::protocol::Event;
        use sentry::protocol::Level;
        use sentry::protocol::Value;

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
        let (attachments, omitted) = self.feedback_attachment_payloads(
            options.include_logs,
            options.extra_attachments,
            options.extra_attachment_paths,
            options.logs_override,
        );
        // Skips are otherwise only logged locally, after the uploaded logs were captured.
        if !omitted.is_empty() {
            event
                .extra
                .insert("omitted_attachments".to_string(), Value::from(omitted));
        }
        envelope.add_item(EnvelopeItem::Event(Box::new(event)));

        let body_capacity = ENVELOPE_OVERHEAD_BYTES
            + attachments
                .iter()
                .map(|attachment| attachment.buffer.len())
                .sum::<usize>();
        let mut body = Vec::with_capacity(body_capacity);
        envelope
            .to_writer(&mut body)
            .map_err(|err| anyhow!("failed to encode feedback envelope: {err}"))?;
        for attachment in attachments {
            attachment
                .to_writer(&mut body)
                .map_err(|err| anyhow!("failed to encode feedback envelope: {err}"))?;
        }
        Ok(body)
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


    /// Borrows captured payloads and owns newly read ones, in upload order.
    fn feedback_attachment_payloads<'a>(
        &'a self,
        include_logs: bool,
        extra_attachments: &'a [FeedbackAttachment],
        extra_attachment_paths: &[FeedbackAttachmentPath],
        logs_override: Option<Vec<u8>>,
    ) -> (Vec<FeedbackUploadAttachment<'a>>, Vec<String>) {

        let mut attachments = Vec::new();
        let mut omitted = Vec::new();
        let mut remaining = MAX_TOTAL_ATTACHMENT_BYTES;

        if include_logs {
            let logs = logs_override.as_deref().unwrap_or(&self.bytes);
            if reserve_attachment_bytes("codex-logs.log", logs.len(), &mut remaining, &mut omitted)
            {
                attachments.push(FeedbackUploadAttachment {
                    buffer: logs_override.map_or_else(
                        || Cow::Borrowed(self.bytes.as_slice()),
                        Cow::Owned,
                    ),
                    filename: Cow::Borrowed("codex-logs.log"),
                    content_type: Some(Cow::Borrowed("text/plain")),
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
                        &mut omitted,
                    )
                })
                .map(|attachment| FeedbackUploadAttachment {
                    buffer: Cow::Borrowed(attachment.buffer.as_slice()),
                    filename: Cow::Borrowed(attachment.filename.as_str()),
                    content_type: attachment.content_type.as_deref().map(Cow::Borrowed),
                }),
        );

        if let Some(text) = self.feedback_diagnostics_attachment_text(include_logs)
            && reserve_attachment_bytes(
                FEEDBACK_DIAGNOSTICS_ATTACHMENT_FILENAME,
                text.len(),
                &mut remaining,
                &mut omitted,
            )
        {
            attachments.push(FeedbackUploadAttachment {
                buffer: Cow::Owned(text.into_bytes()),
                filename: Cow::Borrowed(FEEDBACK_DIAGNOSTICS_ATTACHMENT_FILENAME),
                content_type: Some(Cow::Borrowed("text/plain")),
            });
        }

        for attachment_path in extra_attachment_paths {
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
            let filename = if attachment_path.hydrate_rollout {
                filename
                    .strip_suffix(".zst")
                    .unwrap_or(&filename)
                    .to_string()
            } else {
                filename
            };
            let limit = remaining.min(MAX_ATTACHMENT_BYTES);
            let read = if attachment_path.hydrate_rollout {
                codex_rollout::read_rollout_for_feedback(&attachment_path.path, limit)
            } else {
                fs::File::open(&attachment_path.path).and_then(|file| {
                    let mut data = Vec::new();
                    file.take(limit as u64 + 1).read_to_end(&mut data)?;
                    Ok(data)
                })
            };
            let data = match read {
                Ok(data) => data,
                Err(err) => {
                    tracing::warn!(
                        path = %attachment_path.path.display(),
                        error = %err,
                        "failed to read log attachment; skipping"
                    );
                    omitted.push(format!("{filename}: read failed: {err}"));
                    continue;
                }
            };
            if !reserve_attachment_bytes(&filename, data.len(), &mut remaining, &mut omitted) {
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
            attachments.push(FeedbackUploadAttachment {
                buffer: Cow::Owned(data),
                filename: Cow::Owned(filename),
                content_type: Some(Cow::Owned(content_type)),
            });
        }

        (attachments, omitted)
    }
}

fn reserve_attachment_bytes(
    filename: &str,
    bytes: usize,
    remaining: &mut usize,
    omitted: &mut Vec<String>,
) -> bool {
    if bytes > MAX_ATTACHMENT_BYTES || bytes > *remaining {
        tracing::warn!(
            filename,
            bytes,
            "feedback attachment exceeds upload byte budget; skipping"
        );
        // Path-backed reads stop one byte past the limit, so report the limit, not a size.
        omitted.push(if bytes > MAX_ATTACHMENT_BYTES {
            format!("{filename}: exceeds the {MAX_ATTACHMENT_BYTES}-byte attachment limit")
        } else {
            format!("{filename}: exceeds the remaining {remaining}-byte upload budget")
        });
        return false;
    }
    *remaining -= bytes;
    true
}

/// Deadline for one envelope POST, scaled so large attachments are not cut off on slow links.
fn upload_deadline(body_bytes: usize) -> Duration {
    UPLOAD_BASE_TIMEOUT + Duration::from_secs(body_bytes as u64 / UPLOAD_MIN_BYTES_PER_SEC)
}

/// Sends an encoded envelope and succeeds only when Sentry accepts it.
///
/// The request timeout bounds the whole exchange, so callers are never held past `deadline`.
fn send_envelope(
    client: &BlockingHttpClient,
    dsn: &Dsn,
    body: Vec<u8>,
    deadline: Duration,
) -> Result<()> {
    let user_agent = sentry::ClientOptions::default().user_agent;
    let auth = dsn.to_auth(Some(&user_agent)).to_string();
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::HeaderName::from_static("x-sentry-auth"),
        http::HeaderValue::from_str(&auth)
            .map_err(|err| anyhow!("invalid feedback upload auth header: {err}"))?,
    );
    let response = client
        .post(dsn.envelope_api_url())
        .headers(headers)
        .body(body)
        .timeout(deadline)
        .send()
        .map_err(|err| {
            if err.is_timeout() {
                anyhow!("feedback upload did not finish within {deadline:?}")
            } else {
                anyhow!("feedback upload failed: {}", error_chain(&err))
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("feedback upload was rejected with HTTP {status}"));
    }
    Ok(())
}

/// Formats an error with its sources, which carry the connect/TLS/proxy cause.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
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

