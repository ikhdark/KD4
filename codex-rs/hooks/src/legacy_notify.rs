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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::Result;
    use codex_protocol::ThreadId;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;

    use super::*;
    use crate::HookEventAfterAgent;

    fn after_agent_payload(cwd: &std::path::Path) -> HookPayload {
        HookPayload {
            session_id: ThreadId::new(),
            cwd: cwd
                .to_path_buf()
                .try_into()
                .expect("temporary directory should be absolute"),
            client: None,
            triggered_at: chrono::Utc::now(),
            hook_event: HookEvent::AfterAgent {
                event: HookEventAfterAgent {
                    thread_id: ThreadId::new(),
                    turn_id: "turn-1".to_string(),
                    input_messages: Vec::new(),
                    last_assistant_message: None,
                },
            },
        }
    }

    fn delayed_marker_command(directory: &std::path::Path) -> Vec<String> {
        #[cfg(windows)]
        {
            let script = directory.join("delayed-marker.ps1");
            std::fs::write(directory.join("child.ps1"), concat!(
                "Set-Content -Encoding ascii -LiteralPath (Join-Path $PSScriptRoot 'started.txt') -Value $PID\n",
                "Start-Sleep -Seconds 2\n",
                "Set-Content -LiteralPath (Join-Path $PSScriptRoot 'escaped.txt') -Value escaped\n",
            )).expect("write descendant script");
            std::fs::write(
                &script,
                concat!(
                    "Start-Process powershell.exe -WindowStyle Hidden -ArgumentList ('-NoProfile -File \"' + (Join-Path $PSScriptRoot 'child.ps1') + '\"')\n",
                    "Start-Sleep -Seconds 60\n",
                ),
            )
            .expect("write finalizer test script");
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-File".to_string(),
                script.to_string_lossy().into_owned(),
            ]
        }
        #[cfg(not(windows))]
        {
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "(sleep 2; touch \"$1/escaped.txt\") & printf '%s' $! > \"$1/started.txt\"; sleep 60"
                    .to_string(),
                "codex-hook-test".to_string(),
                directory.to_string_lossy().into_owned(),
            ]
        }
    }

    fn expected_notification_json() -> Value {
        let cwd = test_path_buf("/Users/example/project");
        json!({
            "type": "agent-turn-complete",
            "thread-id": "b5f6c1c2-1111-2222-3333-444455556666",
            "turn-id": "12345",
            "cwd": cwd.display().to_string(),
            "client": "codex-tui",
            "input-messages": ["Rename `foo` to `bar` and update the callsites."],
            "last-assistant-message": "Rename complete and verified `cargo build` succeeds.",
        })
    }

    #[test]
    fn legacy_notify_json_matches_historical_wire_shape() -> Result<()> {
        let payload = HookPayload {
            session_id: ThreadId::new(),
            cwd: test_path_buf("/Users/example/project").abs(),
            client: Some("codex-tui".to_string()),
            triggered_at: chrono::Utc::now(),
            hook_event: HookEvent::AfterAgent {
                event: HookEventAfterAgent {
                    thread_id: ThreadId::from_string("b5f6c1c2-1111-2222-3333-444455556666")
                        .expect("valid thread id"),
                    turn_id: "12345".to_string(),
                    input_messages: vec![
                        "Rename `foo` to `bar` and update the callsites.".to_string(),
                    ],
                    last_assistant_message: Some(
                        "Rename complete and verified `cargo build` succeeds.".to_string(),
                    ),
                },
            },
        };

        let serialized = legacy_notify_json(&payload)?;
        let actual: Value = serde_json::from_str(&serialized)?;
        assert_eq!(actual, expected_notification_json());

        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn long_assistant_messages_stay_within_the_windows_command_line_limit() {
        let directory = tempfile::tempdir().expect("notifier output directory");
        let output_path = directory.path().join("argv.json");
        let argv = windows_notify_test_argv(&output_path);
        for message in [
            "\"".repeat(14_000),
            "\\\"".repeat(14_000),
            "\\\\".repeat(40_000),
            "\0\n\r\t".repeat(10_000),
            "é".repeat(40_000),
            "🦀".repeat(40_000),
            "é".repeat(200_000),
        ] {
            let mut payload = after_agent_payload(directory.path());
            let HookEvent::AfterAgent { event } = &mut payload.hook_event;
            event.input_messages = vec!["review the diff".to_string()];
            event.last_assistant_message = Some(message);
            let original: Value =
                serde_json::from_str(&legacy_notify_json(&payload).expect("serialize"))
                    .expect("JSON");
            let argument =
                legacy_notify_argument(&payload, &argv).expect("payload should still be produced");
            assert!(windows_notify_command_line_len(&argv, &argument) <= MAX_NOTIFY_COMMAND_LINE);
            let mut parsed: Value = serde_json::from_str(&argument).expect("valid JSON");
            assert!(
                parsed["last-assistant-message"].is_string(),
                "the assistant message is retained in truncated form"
            );
            assert!(
                parsed["last-assistant-message"]
                    .as_str()
                    .expect("message")
                    .contains(" chars truncated…")
            );
            parsed["last-assistant-message"] = original["last-assistant-message"].clone();
            assert_eq!(parsed, original, "all other fields must be unchanged");

            // Exercise the real std Windows escaping and the child's argv parser,
            // rather than trusting our length calculation to prove itself.
            let output = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .arg(&argument)
                .output()
                .expect("escaped notification must fit CreateProcessW");
            assert!(output.status.success(), "{output:?}");
            let received: Vec<String> =
                serde_json::from_slice(&std::fs::read(&output_path).expect("child argv"))
                    .expect("argv JSON");
            let mut expected = argv[1..].to_vec();
            expected.push(argument);
            assert_eq!(
                received, expected,
                "Command must deliver exactly one JSON argument"
            );
            std::fs::remove_file(&output_path).expect("remove previous child output");
        }
    }

    #[cfg(windows)]
    fn windows_notify_test_argv(output: &std::path::Path) -> Vec<String> {
        vec![
            std::env::current_exe()
                .expect("test executable")
                .to_string_lossy()
                .into_owned(),
            "--exact".to_string(),
            "legacy_notify::tests::windows_notify_argv_child".to_string(),
            "--nocapture".to_string(),
            "--".to_string(),
            "__codex_notify_argv_child__".to_string(),
            output.to_string_lossy().into_owned(),
            String::new(),
            "fixed \\".to_string(),
            "fixed\\\"quote".to_string(),
            "fixed\t\n🦀".to_string(),
        ]
    }

    #[test]
    #[cfg(windows)]
    fn windows_notify_argv_child() {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        let Some(marker) = arguments
            .iter()
            .position(|argument| argument == "__codex_notify_argv_child__")
        else {
            return;
        };
        std::fs::write(
            &arguments[marker + 1],
            serde_json::to_vec(&arguments).expect("serialize received argv"),
        )
        .expect("write received argv");
    }

    #[test]
    #[cfg(windows)]
    fn windows_notify_limit_matches_actual_command_boundary() {
        let directory = tempfile::tempdir().expect("notifier output directory");
        let output_path = directory.path().join("argv.json");
        let mut argv = windows_notify_test_argv(&output_path);
        argv.push("\"".repeat(6_000));
        let mut payload = after_agent_payload(directory.path());
        let HookEvent::AfterAgent { event } = &mut payload.hook_event;
        event.input_messages = vec!["\\\"🦀".repeat(1_000)];
        event.last_assistant_message = Some(String::new());
        let empty = legacy_notify_json(&payload).expect("empty-message JSON");
        let room = MAX_NOTIFY_COMMAND_LINE - windows_notify_command_line_len(&argv, &empty);
        let HookEvent::AfterAgent { event } = &mut payload.hook_event;
        event.last_assistant_message = Some("a".repeat(room));
        let exact = legacy_notify_json(&payload).expect("boundary JSON");
        assert_eq!(
            legacy_notify_argument(&payload, &argv).expect("fits"),
            exact
        );

        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .arg(&exact)
            .output()
            .expect("32,767 UTF-16 units including NUL must fit");
        assert!(output.status.success(), "{output:?}");
        let received: Vec<String> =
            serde_json::from_slice(&std::fs::read(&output_path).expect("child argv"))
                .expect("argv JSON");
        assert_eq!(received.last(), Some(&exact));

        // The OS, not our counter, independently establishes the boundary.
        let error = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .arg(format!("{exact}a"))
            .output()
            .expect_err("one additional UTF-16 unit must exceed CreateProcessW");
        assert_eq!(error.raw_os_error(), Some(206));
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn windows_notify_rejects_irreducible_command_lines() {
        let directory = tempfile::tempdir().expect("notifier output directory");
        let output = directory.path().join("argv.json");
        for field in ["arguments", "input_messages", "client", "turn_id", "cwd"] {
            let mut argv = windows_notify_test_argv(&output);
            let mut payload = after_agent_payload(directory.path());
            let HookEvent::AfterAgent { event } = &mut payload.hook_event;
            // Quotes expand in both JSON and the Windows command line. These
            // fields are not ours to silently truncate or discard.
            let pressure = "\"".repeat(20_000);
            match field {
                "arguments" => argv.push(pressure),
                "input_messages" => event.input_messages.push(pressure),
                "client" => payload.client = Some(pressure),
                "turn_id" => event.turn_id = pressure,
                "cwd" => {
                    payload.cwd = directory.path().join("a".repeat(33_000)).abs();
                }
                _ => unreachable!(),
            }
            let error = legacy_notify_argument(&payload, &argv).expect_err(field);
            assert!(error.to_string().contains("Windows command-line limit"));
            let result = notify_hook(argv.clone()).execute(&payload).await.result;
            assert!(matches!(result, HookResult::FailedContinue(_)), "{field}");
            let result = mutating_finalizer_hook(argv).execute(&payload).await.result;
            assert!(matches!(result, HookResult::FailedAbort(_)), "{field}");
            assert!(!output.exists(), "oversized notifications must not spawn");
        }
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn windows_mutating_finalizer_delivers_truncated_notification() {
        let directory = tempfile::tempdir().expect("notifier output directory");
        let output = directory.path().join("argv.json");
        let argv = windows_notify_test_argv(&output);
        let mut payload = after_agent_payload(directory.path());
        let HookEvent::AfterAgent { event } = &mut payload.hook_event;
        event.last_assistant_message = Some("\"".repeat(14_000));
        let result = mutating_finalizer_hook(argv.clone())
            .execute(&payload)
            .await
            .result;
        assert!(matches!(result, HookResult::Success));
        let received: Vec<String> =
            serde_json::from_slice(&std::fs::read(output).expect("finalizer argv"))
                .expect("argv JSON");
        assert_eq!(received.len(), argv.len());
        assert_eq!(&received[..received.len() - 1], &argv[1..]);
        let parsed: Value = serde_json::from_str(received.last().expect("payload")).expect("JSON");
        assert!(
            parsed["last-assistant-message"]
                .as_str()
                .expect("message")
                .contains(" chars truncated…")
        );
    }

    #[test]
    fn short_notifications_are_passed_through_unchanged() {
        let payload = HookPayload {
            session_id: ThreadId::new(),
            cwd: test_path_buf("/Users/example/project").abs(),
            client: Some("codex-tui".to_string()),
            triggered_at: chrono::Utc::now(),
            hook_event: HookEvent::AfterAgent {
                event: HookEventAfterAgent {
                    thread_id: ThreadId::from_string("b5f6c1c2-1111-2222-3333-444455556666")
                        .expect("valid thread id"),
                    turn_id: "12345".to_string(),
                    input_messages: vec![
                        "Rename `foo` to `bar` and update the callsites.".to_string(),
                    ],
                    last_assistant_message: Some(
                        "Rename complete and verified `cargo build` succeeds.".to_string(),
                    ),
                },
            },
        };
        let argv = vec!["notify-send".to_string(), "Codex".to_string()];
        let argument = legacy_notify_argument(&payload, &argv).expect("payload");
        let parsed: Value = serde_json::from_str(&argument).expect("valid JSON");
        assert_eq!(parsed, expected_notification_json());
    }

    #[tokio::test]
    async fn mutating_finalizer_dispatch_uses_task_cwd() {
        let directory = tempfile::tempdir().expect("task directory");
        std::fs::write(directory.path().join("finalizer-input.txt"), "input")
            .expect("write task input");
        #[cfg(windows)]
        let argv = {
            let script = directory.path().join("finalize.ps1");
            std::fs::write(
                &script,
                "if ((Get-Content -LiteralPath ./finalizer-input.txt -Raw) -ne 'input') { exit 1 }; Set-Content -LiteralPath ./finalizer-output.txt -NoNewline -Value done",
            )
            .expect("write finalizer script");
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-File".to_string(),
                script.to_string_lossy().into_owned(),
            ]
        };
        #[cfg(not(windows))]
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "test \"$(cat finalizer-input.txt)\" = input && printf done > finalizer-output.txt"
                .to_string(),
        ];
        let hooks = crate::Hooks::new(crate::HooksConfig {
            legacy_notify_argv: Some(argv),
            mutating_finalizer: true,
            ..Default::default()
        });
        let outcomes = hooks.dispatch(after_agent_payload(directory.path())).await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].result, HookResult::Success));
        assert_eq!(
            std::fs::read_to_string(directory.path().join("finalizer-output.txt"))
                .expect("finalizer wrote in task directory"),
            "done"
        );

        let outcomes = hooks
            .dispatch(after_agent_payload(&directory.path().join("missing")))
            .await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].result, HookResult::FailedAbort(_)));
    }

    #[tokio::test]
    async fn cancelling_mutating_finalizer_terminates_its_subprocess() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let started = temp_dir.path().join("started.txt");
        let escaped = temp_dir.path().join("escaped.txt");
        let hook = mutating_finalizer_hook(delayed_marker_command(temp_dir.path()));
        let payload = after_agent_payload(temp_dir.path());

        let hook_task = tokio::spawn(async move { hook.execute(&payload).await });
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&started)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok())
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("finalizer subprocess should start");

        hook_task.abort();
        assert!(
            hook_task
                .await
                .expect_err("aborted finalizer task should not complete")
                .is_cancelled()
        );

        codex_utils_pty::test_support::wait_for_process_exit(pid, Duration::from_secs(2))
            .await
            .expect("cancelled finalizer must terminate its subprocess");
        assert!(
            !escaped.exists(),
            "mutating finalizer subprocess survived cancellation"
        );
    }
}
