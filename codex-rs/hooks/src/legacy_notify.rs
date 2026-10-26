use std::process::Stdio;
use std::sync::Arc;

use serde::Serialize;

use crate::Hook;
use crate::HookEvent;
use crate::HookPayload;
use crate::HookResult;
use crate::command_from_argv;

/// Legacy notify payload appended as the final argv argument for backward compatibility.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum UserNotification<'a> {
    #[serde(rename_all = "kebab-case")]
    AgentTurnComplete {
        thread_id: String,
        turn_id: &'a str,
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client: Option<&'a str>,
        input_messages: &'a [String],
        last_assistant_message: Option<&'a str>,
    },
}

pub fn legacy_notify_json(payload: &HookPayload) -> Result<String, serde_json::Error> {
    let HookEvent::AfterAgent { event } = &payload.hook_event;
    serde_json::to_string(&legacy_notification(payload, event.last_assistant_message.as_deref()))
}

fn legacy_notification<'a>(
    payload: &'a HookPayload,
    last_assistant_message: Option<&'a str>,
) -> UserNotification<'a> {
    let HookEvent::AfterAgent { event } = &payload.hook_event;
    UserNotification::AgentTurnComplete {
        thread_id: event.thread_id.to_string(),
        turn_id: &event.turn_id,
        cwd: payload.cwd.display().to_string(),
        client: payload.client.as_deref(),
        input_messages: &event.input_messages,
        last_assistant_message,
    }
}

/// `CreateProcessW` includes the terminating NUL in its UTF-16 limit.
#[cfg(windows)]
const MAX_NOTIFY_COMMAND_LINE: usize = 32_767;

/// Length after `std::process::Command` escapes a regular Windows argument.
/// Unlike shell-command's renderer, std only surrounds empty arguments or
/// arguments containing spaces/tabs with quotes; internal quotes are always
/// escaped. Count UTF-16 units, including doubled backslashes before quotes.
#[cfg(windows)]
fn windows_argument_len(argument: &str) -> usize {
    let quoted = argument.is_empty() || argument.contains([' ', '\t']);
    let mut length = if quoted { 2 } else { 0 };
    let mut backslashes = 0;
    for unit in argument.encode_utf16() {
        length += 1;
        if unit == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if unit == u16::from(b'"') {
                length += backslashes + 1;
            }
            backslashes = 0;
        }
    }
    length + if quoted { backslashes } else { 0 }
}

/// Native executable command line: argv[0] is always quoted by std, each
/// subsequent argument has a separating space, and the line ends in a NUL.
#[cfg(windows)]
fn windows_notify_command_line_len(argv: &[String], payload: &str) -> usize {
    let Some((program, arguments)) = argv.split_first() else {
        return usize::MAX;
    };
    arguments
        .iter()
        .fold(program.encode_utf16().count() + 3, |length, argument| {
            length.saturating_add(1 + windows_argument_len(argument))
        })
        .saturating_add(1 + windows_argument_len(payload))
}

/// Serialize the notification, shrinking the unbounded free-text fields until
/// the assembled command line fits the platform limit.
///
/// The notifier contract is documented in `Config::notify`: Codex appends
/// exactly one extra argv argument holding the JSON payload. Long turns can push
/// `last_assistant_message` past the Windows command-line cap, and `spawn()`
/// then fails with os error 206 — the hook silently never fires on precisely the
/// longest, most interesting turns. Truncating the free-text field keeps every
/// documented key present and parseable for existing notifiers, which a
/// different wire shape (a temp-file path, or stdin delivery) would not.
fn legacy_notify_argument(payload: &HookPayload, argv: &[String]) -> anyhow::Result<String> {
    #[cfg(windows)]
    {
        truncated_notify_json(payload, argv)
    }
    #[cfg(not(windows))]
    {
        let _ = argv;
        Ok(legacy_notify_json(payload)?)
    }
}

/// Count std's Windows argument escaping while serde emits JSON. UTF-8 lead
/// bytes determine UTF-16 width, so serializer writes may split a code point.
/// Retained JSON stops at the native argument budget. Serde may still scan a
/// borrowed string before writing its first span; no full encoded copy is kept.
#[cfg(windows)]
#[derive(Clone)]
struct NotifyArgumentWriter {
    bytes: Vec<u8>,
    length: usize,
    backslashes: usize,
    quoted: bool,
    limit: usize,
    exceeded: bool,
}

#[cfg(windows)]
impl NotifyArgumentWriter {
    fn new(limit: usize) -> Self {
        Self { bytes: Vec::new(), length: 0, backslashes: 0, quoted: false, limit, exceeded: false }
    }

    fn append_json(&mut self, value: &impl Serialize) -> anyhow::Result<bool> {
        let result = serde_json::to_writer(&mut *self, value);
        if self.exceeded {
            return Ok(false);
        }
        result?;
        Ok(true)
    }

    fn with_message(&self, message: Option<&str>) -> anyhow::Result<Option<String>> {
        use std::io::Write;
        let mut candidate = self.clone();
        if !candidate.append_json(&message)? {
            return Ok(None);
        }
        let closing = candidate.write_all(b"}");
        if candidate.exceeded {
            return Ok(None);
        }
        closing?;
        Ok(Some(String::from_utf8(candidate.bytes)?))
    }
}

#[cfg(windows)]
impl std::io::Write for NotifyArgumentWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for &byte in bytes {
            if byte.is_ascii() {
                self.length += 1;
                if byte == b'\\' {
                    self.backslashes += 1;
                } else {
                    if byte == b'"' {
                        self.length += self.backslashes + 1;
                    }
                    self.backslashes = 0;
                }
                self.quoted |= matches!(byte, b' ' | b'\t');
            } else if byte >= 0xc0 {
                self.length += if byte >= 0xf0 { 2 } else { 1 };
                self.backslashes = 0;
            }
            // Complete JSON ends in `}`, so trailing argument backslashes
            // cannot occur. Do not charge a temporary backslash suffix twice.
            if self.length + (if self.quoted { 2 } else { 0 }) > self.limit {
                self.exceeded = true;
                return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "notification argument budget exceeded"));
            }
            self.bytes.push(byte);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

/// Re-serialize with `last_assistant_message` truncated to fit the command line.
///
/// Preserve user input and metadata. If even a null assistant message cannot
/// fit, report the failure instead of spawning with an oversized/missing event.
#[cfg(windows)]
fn truncated_notify_json(payload: &HookPayload, argv: &[String]) -> anyhow::Result<String> {
    use std::io::Write;
    let HookEvent::AfterAgent { event } = &payload.hook_event;
    let oversized = || anyhow::anyhow!(
        "legacy notify command exceeds the Windows command-line limit of {MAX_NOTIFY_COMMAND_LINE} UTF-16 units even without the assistant message; shorten notify arguments or non-assistant notification fields"
    );
    // An empty regular argument costs two quotes. Remove only that payload
    // cost, keeping argv[0], fixed arguments, separators and the terminal NUL.
    let prefix_length = windows_notify_command_line_len(argv, "").saturating_sub(2);
    let limit = MAX_NOTIFY_COMMAND_LINE.checked_sub(prefix_length).ok_or_else(oversized)?;
    let mut envelope = NotifyArgumentWriter::new(limit + 5);
    if !envelope.append_json(&legacy_notification(payload, None))? {
        return Err(oversized());
    }
    let envelope = String::from_utf8(envelope.bytes)?;
    let prefix = envelope.strip_suffix("null}")
        .ok_or_else(|| anyhow::anyhow!("legacy notification assistant field is not terminal"))?;
    let mut writer = NotifyArgumentWriter::new(limit);
    if writer.write_all(prefix.as_bytes()).is_err() {
        return Err(oversized());
    }
    // Every UTF-8 string needs at least one UTF-16 unit per three bytes,
    // before JSON/argv escaping. Skip provably excluded full-message scans.
    let original = event.last_assistant_message.as_deref();
    if original.is_none_or(|message| message.len() <= limit * 3)
        && let Some(complete) = writer.with_message(original)?
    {
        return Ok(complete);
    }
    // A present empty string can be shorter than null, so try the original
    // message before deciding that the invariant envelope cannot fit.
    let envelope = writer.with_message(None)?.ok_or_else(oversized)?;
    if let Some(message) = event.last_assistant_message.as_deref() {
        // Geometric reduction bounds retries. Check the serialized and escaped
        // result each time: both JSON escaping and the truncation marker cost
        // space, so a raw-byte estimate is not a sufficient stopping condition.
        let mut retained_bytes = (message.len() / 2).min(MAX_NOTIFY_COMMAND_LINE);
        loop {
            let shortened = codex_utils_string::truncate_middle_chars(message, retained_bytes);
            if let Some(candidate) = writer.with_message(Some(&shortened))? {
                return Ok(candidate);
            }
            if retained_bytes == 0 {
                break;
            }
            retained_bytes /= 2;
        }
    }
    Ok(envelope)
}

pub fn notify_hook(argv: Vec<String>) -> Hook {
    let argv = Arc::new(argv);
    Hook {
        name: "legacy_notify".to_string(),
        func: Arc::new(move |payload: &HookPayload| {
            let argv = Arc::clone(&argv);
            Box::pin(async move {
                let mut command = match command_from_argv(&argv) {
                    Some(command) => command,
                    None => return HookResult::Success,
                };
                let notify_payload = match legacy_notify_argument(payload, &argv) {
                    Ok(notify_payload) => notify_payload,
                    Err(err) => return HookResult::FailedContinue(err.into()),
                };
                command.arg(notify_payload);

                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());

                match command.spawn() {
                    Ok(_) => HookResult::Success,
                    Err(err) => HookResult::FailedContinue(err.into()),
                }
            })
        }),
    }
}

pub fn mutating_finalizer_hook(argv: Vec<String>) -> Hook {
    let argv = Arc::new(argv);
    Hook {
        name: "legacy_notify".to_string(),
        func: Arc::new(move |payload: &HookPayload| {
            let argv = Arc::clone(&argv);
            Box::pin(async move {
                let mut command = match command_from_argv(&argv) {
                    Some(command) => command,
                    None => return HookResult::Success,
                };
                let notify_payload = match legacy_notify_argument(payload, &argv) {
                    Ok(notify_payload) => notify_payload,
                    Err(err) => return HookResult::FailedAbort(err.into()),
                };
                command.arg(notify_payload);

                command.current_dir(payload.cwd.as_path());
                let result =
                    crate::engine::command_runner::run_finalizer_command(command, 600).await;
                match (result.exit_code, result.error) {
                    (Some(0), None) => HookResult::Success,
                    (exit_code, error) => HookResult::FailedAbort(
                        std::io::Error::other(error.unwrap_or_else(|| {
                            format!("mutating finalizer exited with code {exit_code:?}")
                        }))
                        .into(),
                    ),
                }
            })
        }),
    }
}
