use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;

use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::strip_user_message_prefix;
use regex::Regex;
use regex::RegexBuilder;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::process::Command;

use super::ARCHIVED_SESSIONS_SUBDIR;
use super::SESSIONS_SUBDIR;
use super::compression;

const MATCH_CONTEXT_BEFORE_CHARS: usize = 48;
const MATCH_CONTEXT_AFTER_CHARS: usize = 96;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PendingFinalItem {
    turn_id: String,
    item_id: String,
}

/// Search matches keyed by the canonical `.jsonl` path for each rollout.
pub type RolloutSearchMatches = HashMap<PathBuf, Option<String>>;

pub async fn search_rollout_paths(
    rg_command: &Path,
    codex_home: &Path,
    archived: bool,
    search_term: &str,
) -> io::Result<HashSet<PathBuf>> {
    Ok(
        search_rollout_matches(rg_command, codex_home, archived, search_term)
            .await?
            .into_keys()
            .collect(),
    )
}

pub async fn search_rollout_matches(
    rg_command: &Path,
    codex_home: &Path,
    archived: bool,
    search_term: &str,
) -> io::Result<RolloutSearchMatches> {
    let root = codex_home.join(if archived {
        ARCHIVED_SESSIONS_SUBDIR
    } else {
        SESSIONS_SUBDIR
    });
    let json_search_term = json_escaped_search_term(search_term)?;
    let Some(plain_matches) = ripgrep_rollout_paths(
        rg_command,
        root.as_path(),
        json_search_term.as_str(),
        search_term,
    )
    .await?
    else {
        return scan_rollout_matches(root.as_path(), json_search_term.as_str(), search_term).await;
    };
    let mut matches = plain_matches;
    for (path, snippet) in
        scan_compressed_rollout_matches(root.as_path(), json_search_term.as_str(), search_term)
            .await?
    {
        insert_rollout_match(&mut matches, path, snippet);
    }
    Ok(matches)
}

async fn ripgrep_rollout_paths(
    rg_command: &Path,
    root: &Path,
    json_search_term: &str,
    search_term: &str,
) -> io::Result<Option<RolloutSearchMatches>> {
    if !tokio::fs::try_exists(root).await.unwrap_or(false) {
        return Ok(Some(HashMap::new()));
    }

    let json_search_term_regex = case_insensitive_literal_regex(json_search_term)?;
    let search_term = case_insensitive_literal_regex(search_term)?;
    let mut command = rollout_ripgrep_command(rg_command, root, json_search_term);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("ripgrep rollout search stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("ripgrep rollout search stderr was not captured"))?;
    let read_matches = read_ripgrep_rollout_matches(stdout, root, &search_term);
    let drain_stderr = drain_and_check_empty(stderr);
    let wait_for_exit = child.wait();
    let (matches, stderr_is_empty, status) =
        tokio::try_join!(read_matches, drain_stderr, wait_for_exit)?;

    if !status.success() {
        if status.code() == Some(1) && stderr_is_empty {
            return Ok(Some(HashMap::new()));
        }

        return Err(io::Error::other(format!(
            "ripgrep rollout search failed under {}",
            root.display()
        )));
    }

    Ok(Some(
        verify_ripgrep_rollout_matches(matches, &json_search_term_regex, &search_term).await?,
    ))
}

async fn verify_ripgrep_rollout_matches(
    candidates: RolloutSearchMatches,
    json_search_term: &Regex,
    search_term: &Regex,
) -> io::Result<RolloutSearchMatches> {
    let mut matches = HashMap::new();
    for path in candidates.into_keys() {
        let (matched, snippet) =
            inspect_rollout_match(path.as_path(), json_search_term, search_term).await?;
        if matched {
            insert_rollout_match(&mut matches, path, snippet);
        }
    }
    Ok(matches)
}

fn rollout_ripgrep_command(rg_command: &Path, root: &Path, json_search_term: &str) -> Command {
    let mut command = Command::new(rg_command);
    command
        .arg("--json")
        .arg("--max-count=1")
        .arg("--fixed-strings")
        .arg("--ignore-case")
        .arg("--no-ignore")
        .arg("--glob")
        .arg("*.jsonl")
        .arg("--")
        .arg(json_search_term)
        .arg(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn read_ripgrep_rollout_matches(
    stdout: impl AsyncRead + Unpin,
    root: &Path,
    search_term: &Regex,
) -> io::Result<RolloutSearchMatches> {
    let mut lines = BufReader::new(stdout).lines();
    let mut matches = HashMap::new();
    while let Some(line) = lines.next_line().await? {
        let Some((path, snippet)) = parse_ripgrep_rollout_match(&line, root, search_term) else {
            continue;
        };
        insert_rollout_match(&mut matches, path, snippet);
    }
    Ok(matches)
}

async fn drain_and_check_empty(mut reader: impl AsyncRead + Unpin) -> io::Result<bool> {
    let mut first_byte = [0_u8; 1];
    if reader.read(&mut first_byte).await? == 0 {
        return Ok(true);
    }

    let mut sink = tokio::io::sink();
    tokio::io::copy(&mut reader, &mut sink).await?;
    Ok(false)
}

fn parse_ripgrep_rollout_match(
    line: &str,
    root: &Path,
    search_term: &Regex,
) -> Option<(PathBuf, Option<String>)> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type")?.as_str()? != "match" {
        return None;
    }
    let data = value.get("data")?;
    let path_text = data.get("path")?.get("text")?.as_str()?;
    let path = PathBuf::from(path_text);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let snippet = data
        .get("lines")
        .and_then(|lines| lines.get("text"))
        .and_then(Value::as_str)
        .and_then(|jsonl_line| content_match_snippet(jsonl_line, search_term));
    Some((path, snippet))
}

fn insert_rollout_match(
    matches: &mut RolloutSearchMatches,
    path: PathBuf,
    snippet: Option<String>,
) {
    let existing = matches.entry(path).or_insert(None);
    if existing.is_none() {
        *existing = snippet;
    }
}

async fn scan_rollout_matches(
    root: &Path,
    json_search_term: &str,
    search_term: &str,
) -> io::Result<RolloutSearchMatches> {
    let mut matches = HashMap::new();
    let mut dirs = vec![root.to_path_buf()];
    let json_search_term = case_insensitive_literal_regex(json_search_term)?;
    let search_term = case_insensitive_literal_regex(search_term)?;

    while let Some(dir) = dirs.pop() {
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                dirs.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(rollout_file) = compression::RolloutFile::from_path(path) else {
                continue;
            };
            let (matched, snippet) =
                inspect_rollout_match(rollout_file.path(), &json_search_term, &search_term).await?;
            if matched {
                insert_rollout_match(
                    &mut matches,
                    compression::plain_rollout_path(rollout_file.path()),
                    snippet,
                );
            }
        }
    }

    Ok(matches)
}

async fn inspect_rollout_match(
    path: &Path,
    json_search_term: &Regex,
    search_term: &Regex,
) -> io::Result<(bool, Option<String>)> {
    let pending_final_items = load_pending_final_items(path).await?;
    let mut lines = compression::open_rollout_line_reader(path).await?;
    let mut matched = false;
    while let Some(line) = lines.next_line().await? {
        if !json_search_term.is_match(line.as_str()) {
            continue;
        }
        if !rollout_line_is_search_visible(line.as_str(), &pending_final_items) {
            continue;
        }
        matched = true;
        if let Some(snippet) = content_match_snippet(line.as_str(), search_term) {
            return Ok((true, Some(snippet)));
        }
    }
    Ok((matched, None))
}

async fn load_pending_final_items(path: &Path) -> io::Result<HashSet<PendingFinalItem>> {
    let Some(codex_home) = codex_home_from_rollout_path(path) else {
        return Ok(HashSet::new());
    };
    let mut lines = compression::open_rollout_line_reader(path).await?;
    let mut thread_id = None;
    while let Some(line) = lines.next_line().await? {
        let Ok(line) = serde_json::from_str::<RolloutLine>(line.trim()) else {
            continue;
        };
        if let RolloutItem::SessionMeta(meta) = line.item {
            thread_id = Some(meta.meta.id);
            break;
        }
    }
    let Some(thread_id) = thread_id else {
        return Ok(HashSet::new());
    };
    let thread_id = thread_id.to_string();
    let evidence_path = codex_home
        .join("task-evidence")
        .join(format!("{thread_id}.json"));
    let Ok(bytes) = tokio::fs::read(evidence_path).await else {
        return Ok(HashSet::new());
    };
    let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(HashSet::new());
    };
    if document.get("thread_id").and_then(Value::as_str) != Some(thread_id.as_str()) {
        return Ok(HashSet::new());
    }
    let Some(pending_finals) = document.get("pending_finals").and_then(Value::as_array) else {
        return Ok(HashSet::new());
    };
    Ok(pending_finals
        .iter()
        .filter(|pending| {
            !(pending
                .get("externally_emitted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && pending
                    .get("externally_completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false))
        })
        .filter_map(|pending| {
            Some(PendingFinalItem {
                turn_id: pending.get("turn_id")?.as_str()?.to_string(),
                item_id: pending.get("item_id")?.as_str()?.to_string(),
            })
        })
        .collect())
}

fn codex_home_from_rollout_path(path: &Path) -> Option<&Path> {
    path.ancestors().find_map(|ancestor| {
        matches!(
            ancestor.file_name().and_then(|name| name.to_str()),
            Some(SESSIONS_SUBDIR | ARCHIVED_SESSIONS_SUBDIR)
        )
        .then(|| ancestor.parent())
        .flatten()
    })
}

pub async fn first_rollout_content_match_snippet(
    path: &Path,
    search_term: &str,
) -> io::Result<Option<String>> {
    let json_search_term = case_insensitive_literal_regex(json_escaped_search_term(search_term)?)?;
    let search_term = case_insensitive_literal_regex(search_term)?;
    let (_, snippet) = inspect_rollout_match(path, &json_search_term, &search_term).await?;
    Ok(snippet)
}

async fn scan_compressed_rollout_matches(
    root: &Path,
    json_search_term: &str,
    search_term: &str,
) -> io::Result<RolloutSearchMatches> {
    let mut matches = HashMap::new();
    let mut dirs = vec![root.to_path_buf()];
    let json_search_term = case_insensitive_literal_regex(json_search_term)?;
    let search_term = case_insensitive_literal_regex(search_term)?;

    while let Some(dir) = dirs.pop() {
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                dirs.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(rollout_file) = compression::RolloutFile::from_path(path) else {
                continue;
            };
            if !rollout_file.is_compressed() {
                continue;
            }
            let (matched, snippet) =
                inspect_rollout_match(rollout_file.path(), &json_search_term, &search_term).await?;
            if matched {
                insert_rollout_match(
                    &mut matches,
                    compression::plain_rollout_path(rollout_file.path()),
                    snippet,
                );
            }
        }
    }

    Ok(matches)
}

fn json_escaped_search_term(search_term: &str) -> io::Result<String> {
    let serialized = serde_json::to_string(search_term).map_err(io::Error::other)?;
    Ok(serialized[1..serialized.len() - 1].to_string())
}

fn case_insensitive_literal_regex(search_term: impl AsRef<str>) -> io::Result<Regex> {
    RegexBuilder::new(regex::escape(search_term.as_ref()).as_str())
        .case_insensitive(true)
        .build()
        .map_err(io::Error::other)
}

fn content_match_snippet(jsonl_line: &str, search_term: &Regex) -> Option<String> {
    let rollout_line = serde_json::from_str::<RolloutLine>(jsonl_line.trim()).ok()?;
    let text = conversation_text_from_item(&rollout_line.item)?;
    excerpt_around_match(text.as_str(), search_term)
}

fn rollout_line_is_search_visible(
    jsonl_line: &str,
    pending_final_items: &HashSet<PendingFinalItem>,
) -> bool {
    let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(jsonl_line.trim()) else {
        return true;
    };
    let RolloutItem::ResponseItem(item) = &rollout_line.item else {
        return true;
    };
    let ResponseItem::Message {
        id: Some(item_id),
        role,
        ..
    } = item
    else {
        return true;
    };
    if role != "assistant" {
        return true;
    }
    let Some(turn_id) = item.turn_id() else {
        return true;
    };
    !pending_final_items.contains(&PendingFinalItem {
        turn_id: turn_id.to_string(),
        item_id: item_id.clone(),
    })
}

fn conversation_text_from_item(item: &RolloutItem) -> Option<String> {
    match item {
        RolloutItem::EventMsg(EventMsg::UserMessage(user)) => user_message_text(&user.message),
        RolloutItem::EventMsg(EventMsg::AgentMessage(agent)) => agent_message_text(&agent.message),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => match &event.item {
            TurnItem::UserMessage(user) => user_message_text(&user.message()),
            TurnItem::AgentMessage(agent) => {
                let text = agent
                    .content
                    .iter()
                    .map(|content| match content {
                        AgentMessageContent::Text { text } => text.as_str(),
                    })
                    .collect::<String>();
                agent_message_text(&text)
            }
            _ => None,
        },
        RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) => {
            let text = content
                .iter()
                .filter_map(content_item_text)
                .collect::<Vec<_>>()
                .join(" ");
            if text.trim().is_empty() || (role != "user" && role != "assistant") {
                None
            } else {
                Some(text)
            }
        }
        RolloutItem::SessionMeta(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::EventMsg(_)
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::WorldState(_) => None,
    }
}

fn user_message_text(message: &str) -> Option<String> {
    let text = strip_user_message_prefix(message);
    (!text.is_empty()).then(|| text.to_string())
}

fn agent_message_text(message: &str) -> Option<String> {
    let text = message.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn content_item_text(item: &ContentItem) -> Option<&str> {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text.as_str()),
        ContentItem::InputImage { .. } => None,
    }
}

fn excerpt_around_match(text: &str, search_term: &Regex) -> Option<String> {
    let normalized = normalize_preview_text(text);
    let matched = search_term.find(normalized.as_str())?;
    let match_start = matched.start();
    let match_end = matched.end();
    let excerpt_start =
        char_start_before(normalized.as_str(), match_start, MATCH_CONTEXT_BEFORE_CHARS);
    let excerpt_end = char_end_after(normalized.as_str(), match_end, MATCH_CONTEXT_AFTER_CHARS);
    let excerpt = normalized[excerpt_start..excerpt_end].trim();
    if excerpt.is_empty() {
        return None;
    }

    let mut snippet = String::new();
    if excerpt_start > 0 {
        snippet.push_str("... ");
    }
    snippet.push_str(excerpt);
    if excerpt_end < normalized.len() {
        snippet.push_str(" ...");
    }
    Some(snippet)
}

fn normalize_preview_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn char_start_before(text: &str, byte_index: usize, chars_before: usize) -> usize {
    text[..byte_index]
        .char_indices()
        .rev()
        .nth(chars_before)
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn char_end_after(text: &str, byte_index: usize, chars_after: usize) -> usize {
    text[byte_index..]
        .char_indices()
        .nth(chars_after)
        .map(|(offset, _)| byte_index.saturating_add(offset))
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ThreadId;
    use codex_protocol::items::AgentMessageItem;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_protocol::protocol::UserMessageEvent;
    use pretty_assertions::assert_eq;
    use std::io::Write as _;

    fn user_rollout_line(timestamp: &str, message: &str) -> String {
        serde_json::to_string(&RolloutLine {
            timestamp: timestamp.to_string(),
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: message.to_string(),
                ..Default::default()
            })),
        })
        .expect("serialize rollout line")
    }

    fn assistant_response_rollout_line(timestamp: &str, id: &str, message: &str) -> String {
        let mut item = ResponseItem::Message {
            id: Some(id.to_string()),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: message.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        item.set_turn_id_if_missing("turn");
        serde_json::to_string(&RolloutLine {
            timestamp: timestamp.to_string(),
            item: RolloutItem::ResponseItem(item),
        })
        .expect("serialize rollout line")
    }

    fn session_meta_rollout_line(timestamp: &str, thread_id: ThreadId) -> String {
        let mut meta = SessionMeta::default();
        meta.id = thread_id;
        meta.session_id = thread_id.into();
        serde_json::to_string(&RolloutLine {
            timestamp: timestamp.to_string(),
            item: RolloutItem::SessionMeta(SessionMetaLine { meta, git: None }),
        })
        .expect("serialize session meta")
    }

    async fn write_pending_final_evidence(
        codex_home: &Path,
        thread_id: ThreadId,
        pending_finals: Value,
    ) {
        let evidence_dir = codex_home.join("task-evidence");
        tokio::fs::create_dir_all(&evidence_dir)
            .await
            .expect("create evidence directory");
        tokio::fs::write(
            evidence_dir.join(format!("{thread_id}.json")),
            serde_json::to_vec(&serde_json::json!({
                "thread_id": thread_id.to_string(),
                "pending_finals": pending_finals,
            }))
            .expect("serialize evidence"),
        )
        .await
        .expect("write evidence");
    }

    fn completed_agent_rollout_line(timestamp: &str, id: &str, message: &str) -> String {
        serde_json::to_string(&RolloutLine {
            timestamp: timestamp.to_string(),
            item: RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: ThreadId::new(),
                turn_id: "turn".to_string(),
                item: TurnItem::AgentMessage(AgentMessageItem {
                    id: id.to_string(),
                    content: vec![AgentMessageContent::Text {
                        text: message.to_string(),
                    }],
                    phase: None,
                    memory_citation: None,
                }),
                completed_at_ms: 0,
            })),
        })
        .expect("serialize rollout line")
    }

    fn ripgrep_match(path: &str, jsonl_line: &str) -> String {
        serde_json::json!({
            "type": "match",
            "data": {
                "path": { "text": path },
                "lines": { "text": format!("{jsonl_line}\n") }
            }
        })
        .to_string()
    }

    #[test]
    fn parse_ripgrep_rollout_match_returns_bounded_plain_snippet() {
        let root = Path::new("sessions");
        let path = "2026/07/09/rollout-test.jsonl";
        let message = format!("{}needle{}", "a".repeat(80), "b".repeat(140));
        let jsonl_line = user_rollout_line("2026-07-09T00:00:00Z", &message);
        let output = ripgrep_match(path, &jsonl_line);
        let search_term = case_insensitive_literal_regex("needle").expect("search regex");

        let (parsed_path, snippet) =
            parse_ripgrep_rollout_match(&output, root, &search_term).expect("match event");
        let snippet = snippet.expect("conversation snippet");

        assert_eq!(parsed_path, root.join(path));
        assert!(snippet.starts_with("... "));
        assert!(snippet.ends_with(" ..."));
        assert!(snippet.contains("needle"));
        assert!(snippet.chars().count() <= 170, "snippet was {snippet:?}");
    }

    #[test]
    fn parse_ripgrep_rollout_match_retains_path_without_conversation_snippet() {
        let root = Path::new("sessions");
        let path = "rollout-metadata-match.jsonl";
        let jsonl_line = user_rollout_line("needle", "unrelated conversation");
        let output = ripgrep_match(path, &jsonl_line);
        let search_term = case_insensitive_literal_regex("needle").expect("search regex");

        assert_eq!(
            parse_ripgrep_rollout_match(&output, root, &search_term),
            Some((root.join(path), None))
        );
    }

    #[test]
    fn insert_rollout_match_upgrades_missing_snippet_without_replacing_first_snippet() {
        let path = PathBuf::from("rollout.jsonl");
        let mut matches = RolloutSearchMatches::new();

        insert_rollout_match(&mut matches, path.clone(), None);
        insert_rollout_match(&mut matches, path.clone(), Some("first".to_string()));
        insert_rollout_match(&mut matches, path.clone(), Some("later".to_string()));

        assert_eq!(matches.get(&path), Some(&Some("first".to_string())));
    }

    #[tokio::test]
    async fn compressed_scan_retains_metadata_only_match() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = tempdir.path().join("sessions");
        std::fs::create_dir_all(&root).expect("create sessions directory");
        let rollout_path =
            root.join("rollout-2026-07-09T00-00-00-00000000-0000-0000-0000-000000000001.jsonl");
        let compressed_path = compression::compressed_rollout_path(&rollout_path);
        let output = std::fs::File::create(&compressed_path).expect("create compressed rollout");
        let mut encoder = zstd::stream::write::Encoder::new(output, 3).expect("create encoder");
        writeln!(
            encoder,
            "{}",
            user_rollout_line("needle", "unrelated conversation")
        )
        .expect("write compressed rollout");
        encoder.finish().expect("finish compressed rollout");

        let matches = scan_compressed_rollout_matches(&root, "needle", "needle")
            .await
            .expect("scan compressed rollouts");

        assert_eq!(matches.get(&rollout_path), Some(&None));
    }

    #[test]
    fn rollout_ripgrep_command_limits_matches_per_file() {
        let command = rollout_ripgrep_command(Path::new("rg"), Path::new("sessions"), "needle");
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "--json",
                "--max-count=1",
                "--fixed-strings",
                "--ignore-case",
                "--no-ignore",
                "--glob",
                "*.jsonl",
                "--",
                "needle",
                "sessions",
            ]
        );
    }

    #[tokio::test]
    async fn read_ripgrep_rollout_matches_keeps_one_path_and_first_snippet() {
        let root = Path::new("sessions");
        let path = "2026/07/09/rollout-test.jsonl";
        let first_line = user_rollout_line("2026-07-09T00:00:00Z", "first needle");
        let later_line = user_rollout_line("2026-07-09T00:00:01Z", "later needle");
        let output = (0..1_000)
            .map(|index| {
                ripgrep_match(
                    path,
                    if index == 0 {
                        first_line.as_str()
                    } else {
                        later_line.as_str()
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let search_term = case_insensitive_literal_regex("needle").expect("search regex");

        let matches = read_ripgrep_rollout_matches(
            std::io::Cursor::new(output.into_bytes()),
            root,
            &search_term,
        )
        .await
        .expect("read ripgrep matches");

        assert_eq!(
            matches,
            HashMap::from([(root.join(path), Some("first needle".to_string()))])
        );
    }

    #[tokio::test]
    async fn metadata_first_ripgrep_match_can_fall_back_to_later_content() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let rollout_path = tempdir.path().join("rollout-test.jsonl");
        let metadata_match = user_rollout_line("needle", "unrelated conversation");
        let content_match = user_rollout_line("2026-07-09T00:00:01Z", "later needle");
        tokio::fs::write(
            &rollout_path,
            format!("{metadata_match}\n{content_match}\n"),
        )
        .await
        .expect("write rollout");
        let output = ripgrep_match(rollout_path.to_string_lossy().as_ref(), &metadata_match);
        let search_term = case_insensitive_literal_regex("needle").expect("search regex");

        let matches = read_ripgrep_rollout_matches(
            std::io::Cursor::new(output.into_bytes()),
            tempdir.path(),
            &search_term,
        )
        .await
        .expect("read ripgrep matches");

        assert_eq!(matches.get(&rollout_path), Some(&None));
        assert_eq!(
            first_rollout_content_match_snippet(&rollout_path, "needle")
                .await
                .expect("scan rollout fallback"),
            Some("later needle".to_string())
        );
    }

    #[tokio::test]
    async fn provisional_raw_assistant_message_is_not_search_visible() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let sessions = tempdir.path().join(SESSIONS_SUBDIR);
        tokio::fs::create_dir_all(&sessions)
            .await
            .expect("create sessions directory");
        let rollout_path = sessions.join("rollout-test.jsonl");
        let thread_id = ThreadId::new();
        let session_meta = session_meta_rollout_line("2026-07-09T00:00:00Z", thread_id);
        let provisional =
            assistant_response_rollout_line("2026-07-09T00:00:01Z", "msg_1", "private needle");
        tokio::fs::write(&rollout_path, format!("{session_meta}\n{provisional}\n"))
            .await
            .expect("write rollout");
        write_pending_final_evidence(
            tempdir.path(),
            thread_id,
            serde_json::json!([{
                "turn_id": "turn",
                "item_id": "msg_1",
                "persisted": true,
                "externally_emitted": false,
                "externally_completed": false,
            }]),
        )
        .await;
        let json_search_term = case_insensitive_literal_regex("needle").expect("json search regex");
        let search_term = case_insensitive_literal_regex("needle").expect("search regex");

        assert_eq!(
            inspect_rollout_match(&rollout_path, &json_search_term, &search_term)
                .await
                .expect("inspect rollout"),
            (false, None)
        );

        let verified = verify_ripgrep_rollout_matches(
            HashMap::from([(rollout_path.clone(), Some("private needle".to_string()))]),
            &json_search_term,
            &search_term,
        )
        .await
        .expect("verify ripgrep candidates");
        assert!(!verified.contains_key(&rollout_path));
    }

    #[tokio::test]
    async fn non_pending_raw_assistant_message_remains_search_visible() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let sessions = tempdir.path().join(SESSIONS_SUBDIR);
        tokio::fs::create_dir_all(&sessions)
            .await
            .expect("create sessions directory");
        let rollout_path = sessions.join("rollout-test.jsonl");
        let thread_id = ThreadId::new();
        let session_meta = session_meta_rollout_line("2026-07-09T00:00:00Z", thread_id);
        let assistant =
            assistant_response_rollout_line("2026-07-09T00:00:01Z", "msg_visible", "public needle");
        tokio::fs::write(&rollout_path, format!("{session_meta}\n{assistant}\n"))
            .await
            .expect("write rollout");
        write_pending_final_evidence(
            tempdir.path(),
            thread_id,
            serde_json::json!([
                {
                    "turn_id": "different_turn",
                    "item_id": "msg_visible",
                    "persisted": true,
                    "externally_emitted": false,
                    "externally_completed": false,
                },
                {
                    "turn_id": "turn",
                    "item_id": "different_item",
                    "persisted": true,
                    "externally_emitted": false,
                    "externally_completed": false,
                }
            ]),
        )
        .await;

        assert_eq!(
            first_rollout_content_match_snippet(&rollout_path, "needle")
                .await
                .expect("search rollout"),
            Some("public needle".to_string())
        );
    }

    #[tokio::test]
    async fn fully_emitted_final_raw_assistant_message_is_search_visible() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let sessions = tempdir.path().join(SESSIONS_SUBDIR);
        tokio::fs::create_dir_all(&sessions)
            .await
            .expect("create sessions directory");
        let rollout_path = sessions.join("rollout-test.jsonl");
        let thread_id = ThreadId::new();
        let session_meta = session_meta_rollout_line("2026-07-09T00:00:00Z", thread_id);
        let assistant =
            assistant_response_rollout_line("2026-07-09T00:00:01Z", "msg_1", "public needle");
        tokio::fs::write(&rollout_path, format!("{session_meta}\n{assistant}\n"))
            .await
            .expect("write rollout");
        write_pending_final_evidence(
            tempdir.path(),
            thread_id,
            serde_json::json!([{
                "turn_id": "turn",
                "item_id": "msg_1",
                "persisted": true,
                "externally_emitted": true,
                "externally_completed": true,
            }]),
        )
        .await;

        assert_eq!(
            first_rollout_content_match_snippet(&rollout_path, "needle")
                .await
                .expect("search rollout"),
            Some("public needle".to_string())
        );
    }

    #[tokio::test]
    async fn completed_agent_lifecycle_message_remains_search_visible() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let rollout_path = tempdir.path().join("rollout-test.jsonl");
        let provisional =
            assistant_response_rollout_line("2026-07-09T00:00:00Z", "msg_1", "public needle");
        let completed =
            completed_agent_rollout_line("2026-07-09T00:00:01Z", "msg_1", "public needle");
        tokio::fs::write(&rollout_path, format!("{provisional}\n{completed}\n"))
            .await
            .expect("write rollout");

        assert_eq!(
            first_rollout_content_match_snippet(&rollout_path, "needle")
                .await
                .expect("search rollout"),
            Some("public needle".to_string())
        );
    }

    #[test]
    fn legacy_agent_message_remains_search_visible() {
        let item = RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
            message: "legacy answer".to_string(),
            phase: None,
            memory_citation: None,
        }));

        assert_eq!(
            conversation_text_from_item(&item),
            Some("legacy answer".to_string())
        );
    }
}
