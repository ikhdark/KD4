use super::ConversationMessage;
use super::ExternalAgentSessionMigration;
use super::MessageRole;
use super::title::IMPORTED_SESSION_FALLBACK_TITLE;
use super::title::SessionTitleCandidates;
use super::title::fallback_title_from_user_message;
use super::truncate;
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
    let mut reader = BufReader::new(file);
    let mut cwd = None;
    let mut custom_title = None;
    let mut ai_title = None;
    let mut fallback_title = None;
    let mut saw_user_message = false;
    let mut latest_timestamp = None;
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
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
                && let Some(message) = conversation_message_from_owned_record(&mut record)
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

pub(super) fn read_session_import(path: &Path, cancellation: &tokio_util::sync::CancellationToken) -> io::Result<ParsedSessionImport> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut cwd = None;
    let mut custom_title = None;
    let mut ai_title = None;
    let mut messages = Vec::new();
    let mut line = String::new();
    let mut hasher = Sha256::new();
    loop {
        super::super::files::check_cancel(cancellation)?;
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
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
        if let Some(message) = conversation_message_from_owned_record(&mut record) {
            messages.push(message);
        }
    }
    Ok(ParsedSessionImport {
        cwd,
        custom_title,
        ai_title,
        messages,
        content_sha256: hex::encode(hasher.finalize()),
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
                        only_tool_result &= text.trim().is_empty();
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

fn conversation_message_from_owned_record(record: &mut JsonValue) -> Option<ConversationMessage> {
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
        content => extract_message_text(&content)?,
    };
    Some(ConversationMessage {
        role,
        text: extracted,
        timestamp,
    })
}

fn extract_message_text(content: &JsonValue) -> Option<String> {
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
                parts.push(tool_result_note(block));
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

fn tool_result_note(block: &JsonValue) -> String {
    let label = if block.get("is_error").and_then(JsonValue::as_bool) == Some(true) {
        format!("[{EXTERNAL_AGENT_TOOL_RESULT_TAG}: error]")
    } else {
        format!("[{EXTERNAL_AGENT_TOOL_RESULT_TAG}]")
    };
    let text = tool_result_text(block.get("content"));
    let text = match tool_id_note(block, "tool_use_id") {
        Some(id) if text.is_empty() => id,
        Some(id) => format!("{id}\n{text}"),
        None => text,
    };
    if text.is_empty() {
        format!("{label}\n[/{EXTERNAL_AGENT_TOOL_RESULT_TAG}]")
    } else {
        format!("{label}\n{text}\n[/{EXTERNAL_AGENT_TOOL_RESULT_TAG}]")
    }
}

fn tool_id_note(block: &JsonValue, field: &str) -> Option<String> {
    let id = block.get(field)?.as_str()?;
    // Quote source IDs so embedded newlines cannot masquerade as note fields.
    let id = JsonValue::String(truncate(id, TOOL_ID_MAX_LEN));
    Some(format!("call_id: {id}"))
}

fn tool_result_text(content: Option<&JsonValue>) -> String {
    match content {
        Some(JsonValue::String(text)) => truncate(text, TOOL_RESULT_MAX_LEN),
        Some(JsonValue::Array(items)) => {
            let mut texts = items
                .iter()
                .filter_map(|item| item.get("text").and_then(JsonValue::as_str))
                .filter(|text| !text.is_empty());
            let first = texts.next().unwrap_or_default();
            let excerpt = first
                .chars()
                .chain(texts.flat_map(|text| std::iter::once('\n').chain(text.chars())))
                .take(TOOL_RESULT_MAX_LEN + 1)
                .collect::<String>();
            truncate(&excerpt, TOOL_RESULT_MAX_LEN)
        }
        _ => String::new(),
    }
}

fn parse_timestamp(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|value| value.timestamp())
}

