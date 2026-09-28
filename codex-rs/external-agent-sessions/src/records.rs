use crate::ConversationMessage;
use crate::ExternalAgentSessionMigration;
use crate::MessageRole;
use crate::title::IMPORTED_SESSION_FALLBACK_TITLE;
use crate::title::SessionTitleCandidates;
use crate::title::fallback_title_from_user_message;
use crate::truncate;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

const NOTE_MAX_LEN: usize = 2_000;
const TOOL_NAME_MAX_LEN: usize = 120;
const TOOL_ID_MAX_LEN: usize = 200;
const TOOL_RESULT_MAX_LEN: usize = 4_000;
const TOOL_RESULT_OMISSION: &str =
    "\n[... middle omitted; original record is identified below ...]\n";
const EXTERNAL_AGENT_TOOL_CALL_TAG: &str = "external_agent_tool_call";
const EXTERNAL_AGENT_TOOL_RESULT_TAG: &str = "external_agent_tool_result";

pub struct SessionSummary {
    pub latest_timestamp: i64,
    pub migration: ExternalAgentSessionMigration,
}

pub(super) struct ParsedSessionImport {
    pub cwd: Option<PathBuf>,
    pub custom_title: Option<String>,
    pub ai_title: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub content_sha256: String,
}

pub fn summarize_session(path: &Path) -> io::Result<Option<SessionSummary>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut cwd = None;
    let mut custom_title = None;
    let mut ai_title = None;
    let mut fallback_title = None;
    let mut saw_user_message = false;
    let mut latest_timestamp = None;

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(mut record) = serde_json::from_str::<JsonValue>(trimmed) else {
            continue;
        };
        if cwd.is_none() {
            cwd = record
                .get("cwd")
                .and_then(JsonValue::as_str)
                .map(PathBuf::from);
        }
        if let Some(title) = custom_title_from_record(&record) {
            custom_title = Some(title.to_string());
        }
        if let Some(title) = ai_title_from_record(&record) {
            ai_title = Some(title.to_string());
        }
        let Some(role) = conversation_message_role(&record) else {
            continue;
        };
        if role == MessageRole::User {
            saw_user_message = true;
            if fallback_title.is_none()
                && let Some(message) =
                    conversation_message_from_owned_record(&mut record, &mut false)
            {
                fallback_title = fallback_title_from_user_message(&message.text);
            }
        }
        if let Some(timestamp) = record
            .get("timestamp")
            .and_then(JsonValue::as_str)
            .and_then(parse_timestamp)
        {
            latest_timestamp =
                Some(latest_timestamp.map_or(timestamp, |current: i64| current.max(timestamp)));
        }
    }

    let Some(cwd) = cwd else {
        return Ok(None);
    };
    if !saw_user_message {
        return Ok(None);
    }
    let Some(latest_timestamp) = latest_timestamp else {
        return Ok(None);
    };
    Ok(Some(SessionSummary {
        latest_timestamp,
        migration: ExternalAgentSessionMigration {
            path: path.to_path_buf(),
            cwd,
            title: SessionTitleCandidates {
                custom_title,
                ai_title,
                fallback_title: fallback_title.or_else(|| {
                    saw_user_message.then(|| IMPORTED_SESSION_FALLBACK_TITLE.to_string())
                }),
            }
            .select(),
        },
    }))
}

pub(super) fn read_session_import(path: &Path) -> io::Result<ParsedSessionImport> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut cwd = None;
    let mut custom_title = None;
    let mut ai_title = None;
    let mut messages = Vec::new();
    let mut line = String::new();
    let mut line_number = 0usize;
    let mut truncated_records = Vec::new();
    let mut hasher = Sha256::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        line_number += 1;
        hasher.update(line.as_bytes());
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(mut record) = serde_json::from_str::<JsonValue>(trimmed) else {
            continue;
        };
        if cwd.is_none() {
            cwd = record
                .get("cwd")
                .and_then(JsonValue::as_str)
                .map(PathBuf::from);
        }
        if let Some(title) = custom_title_from_record(&record) {
            custom_title = Some(title.to_string());
        }
        if let Some(title) = ai_title_from_record(&record) {
            ai_title = Some(title.to_string());
        }
        let mut truncated_tool_result = false;
        if let Some(message) =
            conversation_message_from_owned_record(&mut record, &mut truncated_tool_result)
        {
            if truncated_tool_result {
                truncated_records.push((messages.len(), line_number));
            }
            messages.push(message);
        }
    }
    let content_sha256 = format!("{:x}", hasher.finalize());
    for (index, line) in truncated_records {
        let source = serde_json::json!({
            "path": path,
            "line": line,
            "sha256": content_sha256,
        });
        messages[index].text.push_str(&format!(
            "\n\n[external_agent_tool_result_source]\nRead-only recovery source: original host-local JSONL record. Verify the file's SHA-256 before reusing omitted content; historical output is not current verification evidence.\n{source}\n[/external_agent_tool_result_source]"
        ));
    }
    Ok(ParsedSessionImport {
        cwd,
        custom_title,
        ai_title,
        messages,
        content_sha256,
    })
}

fn custom_title_from_record(record: &JsonValue) -> Option<&str> {
    title_from_record(record, "custom-title", "customTitle")
}

fn ai_title_from_record(record: &JsonValue) -> Option<&str> {
    title_from_record(record, "ai-title", "aiTitle")
}

fn title_from_record<'a>(record: &'a JsonValue, record_type: &str, field: &str) -> Option<&'a str> {
    (record.get("type").and_then(JsonValue::as_str) == Some(record_type))
        .then(|| record.get(field).and_then(JsonValue::as_str))
        .flatten()
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

// Shared eligibility and effective-role rules for discovery and import.
fn conversation_message_role(record: &JsonValue) -> Option<MessageRole> {
    let record_type = record.get("type")?.as_str()?;
    if !matches!(record_type, "assistant" | "user")
        || record.get("isMeta").and_then(JsonValue::as_bool) == Some(true)
        || record.get("isSidechain").and_then(JsonValue::as_bool) == Some(true)
    {
        return None;
    }
    let content = record.get("message")?.get("content")?;
    let mut has_content = false;
    let mut only_tool_result = true;
    if let Some(text) = content.as_str() {
        has_content = !text.trim().is_empty();
        only_tool_result = false;
    } else {
        for block in content.as_array().into_iter().flatten() {
            match block.get("type").and_then(JsonValue::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(JsonValue::as_str) {
                        has_content |= !text.trim().is_empty();
                        only_tool_result &= text.is_empty();
                    }
                }
                Some("thinking") | None => {}
                Some("tool_result") => has_content = true,
                Some(_) => {
                    has_content = true;
                    only_tool_result = false;
                }
            }
        }
    }
    has_content.then_some(if record_type == "assistant" || only_tool_result {
        MessageRole::Assistant
    } else {
        MessageRole::User
    })
}

fn conversation_message_from_owned_record(
    record: &mut JsonValue,
    truncated_tool_result: &mut bool,
) -> Option<ConversationMessage> {
    let role = conversation_message_role(record)?;
    let timestamp = record
        .get("timestamp")
        .and_then(JsonValue::as_str)
        .and_then(parse_timestamp);
    let content = record.get_mut("message")?.get_mut("content")?.take();
    let extracted = match content {
        JsonValue::String(text) => {
            if text.trim().is_empty() {
                return None;
            }
            text
        }
        content => extract_message_text(&content, truncated_tool_result)?,
    };
    Some(ConversationMessage {
        role,
        text: extracted,
        timestamp,
    })
}

fn extract_message_text(content: &JsonValue, truncated_tool_result: &mut bool) -> Option<String> {
    let blocks = content.as_array()?;
    let mut parts = Vec::new();

    for block in blocks {
        let block_type = block.get("type").and_then(JsonValue::as_str);
        match block_type {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(JsonValue::as_str)
                    && !text.is_empty()
                {
                    parts.push(text.to_string());
                }
            }
            Some("tool_use") => {
                parts.push(tool_call_note(block));
            }
            Some("tool_result") => {
                let (note, truncated) = tool_result_note(block);
                *truncated_tool_result |= truncated;
                parts.push(note);
            }
            Some("thinking") => {}
            Some(other) => {
                parts.push(format!("[external unsupported block: {other}]"));
            }
            None => {}
        }
    }

    let text = parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() { None } else { Some(text) }
}

fn tool_call_note(block: &JsonValue) -> String {
    let name = block
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown");
    let mut lines = Vec::new();
    if let Some(id) = tool_id_note(block, "id") {
        lines.push(id);
    }
    if let Some(input) = block.get("input")
        && input.is_object()
    {
        let input_start = lines.len();
        if let Some(description) = input.get("description").and_then(JsonValue::as_str) {
            lines.push(format!(
                "description: {}",
                truncate(description, NOTE_MAX_LEN)
            ));
        }
        if let Some(command) = input.get("command").and_then(JsonValue::as_str) {
            lines.push(format!("command: {}", truncate(command, NOTE_MAX_LEN)));
        }
        if let Some(file) = input
            .get("file_path")
            .or_else(|| input.get("file"))
            .and_then(JsonValue::as_str)
        {
            lines.push(format!("file: {}", truncate(file, NOTE_MAX_LEN)));
        }
        if lines.len() == input_start {
            lines.push(format!(
                "input: {}",
                truncate(&input.to_string(), NOTE_MAX_LEN)
            ));
        }
    } else if let Some(input) = block.get("input") {
        lines.push(format!(
            "input: {}",
            truncate(&input.to_string(), NOTE_MAX_LEN)
        ));
    }
    bounded_tool_call_note(name, &lines.join("\n"))
}

fn bounded_tool_call_note(name: &str, body: &str) -> String {
    let name = truncate(name, TOOL_NAME_MAX_LEN);
    let opening = format!("[{EXTERNAL_AGENT_TOOL_CALL_TAG}: {name}]");
    let closing = format!("[/{EXTERNAL_AGENT_TOOL_CALL_TAG}]");
    if body.is_empty() {
        return format!("{opening}\n{closing}");
    }
    let fixed_len = opening
        .chars()
        .count()
        .saturating_add(closing.chars().count())
        .saturating_add(2);
    let body = truncate(body, NOTE_MAX_LEN.saturating_sub(fixed_len));
    format!("{opening}\n{body}\n{closing}")
}

fn tool_result_note(block: &JsonValue) -> (String, bool) {
    let label = if block.get("is_error").and_then(JsonValue::as_bool) == Some(true) {
        format!("[{EXTERNAL_AGENT_TOOL_RESULT_TAG}: error]")
    } else {
        format!("[{EXTERNAL_AGENT_TOOL_RESULT_TAG}]")
    };
    let (text, truncated) = tool_result_text(block.get("content"));
    let text = match tool_id_note(block, "tool_use_id") {
        Some(id) if text.is_empty() => id,
        Some(id) => format!("{id}\n{text}"),
        None => text,
    };
    let note = if text.is_empty() {
        format!("{label}\n[/{EXTERNAL_AGENT_TOOL_RESULT_TAG}]")
    } else {
        format!("{label}\n{text}\n[/{EXTERNAL_AGENT_TOOL_RESULT_TAG}]")
    };
    (note, truncated)
}

fn tool_id_note(block: &JsonValue, field: &str) -> Option<String> {
    let id = block.get(field)?.as_str()?;
    // Quote source IDs so embedded newlines cannot masquerade as note fields.
    let id = JsonValue::String(truncate(id, TOOL_ID_MAX_LEN));
    Some(format!("call_id: {id}"))
}

fn tool_result_text(content: Option<&JsonValue>) -> (String, bool) {
    match content {
        Some(JsonValue::String(text)) => tool_result_excerpt(text.chars()),
        Some(JsonValue::Array(items)) => {
            let mut texts = items
                .iter()
                .filter_map(|item| item.get("text").and_then(JsonValue::as_str))
                .filter(|text| !text.is_empty());
            let first = texts.next().unwrap_or_default();
            tool_result_excerpt(
                first
                    .chars()
                    .chain(texts.flat_map(|text| std::iter::once('\n').chain(text.chars()))),
            )
        }
        _ => (String::new(), false),
    }
}

fn tool_result_excerpt(chars: impl DoubleEndedIterator<Item = char> + Clone) -> (String, bool) {
    let mut prefix = chars
        .clone()
        .take(TOOL_RESULT_MAX_LEN + 1)
        .collect::<Vec<_>>();
    if prefix.len() <= TOOL_RESULT_MAX_LEN {
        return (prefix.into_iter().collect(), false);
    }
    let available = TOOL_RESULT_MAX_LEN - TOOL_RESULT_OMISSION.chars().count();
    let head_len = available / 2;
    prefix.truncate(head_len);
    #[expect(clippy::needless_collect, reason = "reversing Take requires an exact-size iterator; the input need not be exact-size")]
    let tail = chars.rev().take(available - head_len).collect::<Vec<_>>();
    let text = prefix
        .into_iter()
        .chain(TOOL_RESULT_OMISSION.chars())
        .chain(tail.into_iter().rev())
        .collect();
    (text, true)
}

fn parse_timestamp(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|value| value.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn reads_session_content_and_matching_fingerprint() {
        let root = TempDir::new().expect("tempdir");
        let path = root.path().join("session.jsonl");
        let contents = [
            serde_json::json!({
                "type": "user",
                "cwd": root.path(),
                "timestamp": "2026-06-03T12:00:00Z",
                "message": { "content": "first request" },
            })
            .to_string(),
            "not json".to_string(),
            serde_json::json!({
                "type": "ai-title",
                "aiTitle": "generated title",
            })
            .to_string(),
            serde_json::json!({
                "type": "custom-title",
                "customTitle": "custom title",
            })
            .to_string(),
        ]
        .join("\n");
        std::fs::write(&path, &contents).expect("session");

        let parsed = read_session_import(&path).expect("parse session");

        assert_eq!(parsed.cwd.as_deref(), Some(root.path()));
        assert_eq!(parsed.custom_title.as_deref(), Some("custom title"));
        assert_eq!(parsed.ai_title.as_deref(), Some("generated title"));
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].text, "first request");
        assert_eq!(
            parsed.content_sha256,
            format!("{:x}", Sha256::digest(contents))
        );
    }

    #[test]
    fn converts_tool_use_blocks_to_bounded_external_agent_tags() {
        let block = serde_json::json!({
            "type": "tool_use",
            "name": "Bash",
            "input": {
                "description": "Check repo status",
                "command": "git status --short"
            }
        });

        assert_eq!(
            tool_call_note(&block),
            "[external_agent_tool_call: Bash]\n\
             description: Check repo status\n\
             command: git status --short\n\
             [/external_agent_tool_call]"
        );
    }

    #[test]
    fn bounds_oversized_recognized_tool_call_fields_and_preserves_closing_tag() {
        let block = serde_json::json!({
            "type": "tool_use",
            "id": "\n".repeat(TOOL_ID_MAX_LEN * 2),
            "name": "B".repeat(TOOL_NAME_MAX_LEN * 2),
            "input": {
                "command": "x".repeat(NOTE_MAX_LEN * 2),
            }
        });

        let note = tool_call_note(&block);

        assert!(note.chars().count() <= NOTE_MAX_LEN);
        assert!(note.contains(&format!(
            "call_id: \"{}...\"",
            "\\n".repeat(TOOL_ID_MAX_LEN - 3)
        )));
        assert!(note.contains("\ncommand: "));
        assert!(note.ends_with("[/external_agent_tool_call]"));
    }

    #[test]
    fn converts_tool_result_blocks_to_bounded_external_agent_tags() {
        let block = serde_json::json!({
            "type": "tool_result",
            "content": "codex-rs/external-agent-sessions/src/records.rs"
        });

        assert_eq!(
            tool_result_note(&block).0,
            "[external_agent_tool_result]\n\
             codex-rs/external-agent-sessions/src/records.rs\n\
             [/external_agent_tool_result]"
        );
    }

    #[test]
    fn converts_error_tool_result_blocks_to_bounded_external_agent_tags() {
        let block = serde_json::json!({
            "type": "tool_result",
            "is_error": true,
            "content": "command failed"
        });

        assert_eq!(
            tool_result_note(&block).0,
            "[external_agent_tool_result: error]\n\
             command failed\n\
             [/external_agent_tool_result]"
        );
    }
    #[test]
    fn discovery_and_import_share_eligibility_and_bounded_tool_result_text() {
        let root = tempfile::TempDir::new().unwrap();
        let path = root.path().join("session.jsonl");
        let result = serde_json::json!({"type":"user", "cwd":root.path(), "timestamp":"2026-06-03T12:00:00Z",
            "message":{"content":[{"type":"tool_result", "is_error":true,
                "content":[{"text":"prefix"},{"text":""},{"text":"界".repeat(10_000)}]}]}});
        std::fs::write(&path, result.to_string()).unwrap();
        assert!(summarize_session(&path).unwrap().is_none());
        assert!(
            crate::prepare_validated_session_import(
                root.path(),
                ExternalAgentSessionMigration {
                    path: path.clone(),
                    cwd: root.path().to_path_buf(),
                    title: None,
                }
            )
            .unwrap()
            .is_none()
        );
        let user = serde_json::json!({"type":"user", "cwd":root.path(), "timestamp":"2026-06-03T12:00:00Z", "message":{"content":"real request"}});
        std::fs::write(&path, format!("{user}\n{result}")).unwrap();
        assert_eq!(
            summarize_session(&path)
                .unwrap()
                .unwrap()
                .migration
                .title
                .as_deref(),
            Some("real request")
        );
        let parsed = read_session_import(&path).unwrap();
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].role, MessageRole::User);
        assert_eq!(parsed.messages[1].role, MessageRole::Assistant);
        let text = &parsed.messages[1].text;
        assert!(text.starts_with("[external_agent_tool_result: error]\nprefix\n"));
        assert!(text.contains(TOOL_RESULT_OMISSION));
        assert!(text.contains("界\n[/external_agent_tool_result]"));
        assert!(text.contains("[external_agent_tool_result_source]"));
    }

    #[test]
    fn imported_excerpts_preserve_results_and_recover_the_exact_original_record() {
        use codex_protocol::models::ContentItem;
        use codex_protocol::models::ResponseItem;
        use codex_protocol::protocol::RolloutItem;

        let root = TempDir::new().unwrap();
        let path = root.path().join("session.jsonl");
        let head = "cargo test: selected workspace";
        let tail = "test result: FAILED. 29 passed; 1 failed. exit_code=101";
        let middle = "OMITTED_DIAGNOSTIC_界🚀".repeat(500);
        for content in [
            serde_json::json!(format!("{head}\n{middle}\n{tail}")),
            serde_json::json!([{"text":head}, {"text":""}, {"text":middle}, {"text":tail}]),
        ] {
            let user = serde_json::json!({"type":"user", "cwd":root.path(), "message":{"content":"Fix the test failure"}});
            let result = serde_json::json!({"type":"user", "message":{"content":[{
                "type":"tool_result", "tool_use_id":"test-call", "is_error":true, "content":content,
            }]}});
            let original = format!("\nnot-json\n{user}\n{result}\n");
            std::fs::write(&path, &original).unwrap();
            let pending = crate::prepare_validated_session_import(
                root.path(),
                ExternalAgentSessionMigration {
                    path: path.clone(),
                    cwd: root.path().to_path_buf(),
                    title: None,
                },
            )
            .unwrap()
            .unwrap();
            let visible = pending
                .session
                .rollout_items
                .iter()
                .filter_map(|item| match item {
                    RolloutItem::ResponseItem(ResponseItem::Message { content, .. }) => {
                        Some(content)
                    }
                    _ => None,
                })
                .flatten()
                .filter_map(|item| match item {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(visible.contains(head));
            assert!(visible.contains(tail));
            assert!(visible.contains("call_id: \"test-call\""));
            assert!(visible.contains("external_agent_tool_result: error"));
            assert!(!visible.contains(&middle));
            let (excerpt, truncated) = tool_result_text(Some(&content));
            assert!(truncated);
            assert!(excerpt.chars().count() <= TOOL_RESULT_MAX_LEN);

            let source = visible
                .lines()
                .filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
                .find(|value| value.get("sha256").is_some())
                .unwrap();
            let recovery_path = PathBuf::from(source["path"].as_str().unwrap());
            let recovered = std::fs::read(&recovery_path).unwrap();
            assert_eq!(
                source["sha256"],
                format!("{:x}", Sha256::digest(&recovered))
            );
            assert_eq!(source["sha256"], pending.source_content_sha256);
            assert_eq!(source["line"], 4);
            let recovered_record: JsonValue = serde_json::from_str(
                std::str::from_utf8(&recovered)
                    .unwrap()
                    .lines()
                    .nth(3)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                recovered_record, result,
                "locator must recover the omitted diagnostic, not an adjacent record"
            );
        }
    }

    #[test]
    fn short_tool_results_do_not_claim_omission_or_add_recovery_work() {
        for size in [0, TOOL_RESULT_MAX_LEN - 1, TOOL_RESULT_MAX_LEN] {
            let text = "界".repeat(size);
            let (excerpt, truncated) = tool_result_text(Some(&serde_json::json!(text)));
            assert_eq!(excerpt, text);
            assert!(!truncated);
        }
    }
}
