use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::USER_MESSAGE_BEGIN;
use regex::Regex;
use regex::RegexBuilder;
use serde_json::Value;
use tokio::process::Command;

use super::ARCHIVED_SESSIONS_SUBDIR;
use super::SESSIONS_SUBDIR;
use super::compression;
use super::list;
use super::list::ThreadListLayout;

const MATCH_CONTEXT_BEFORE_CHARS: usize = 48;
const MATCH_CONTEXT_AFTER_CHARS: usize = 96;
const RIPGREP_PATH_BATCH_SIZE: usize = 128;

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
    let layout = if archived {
        ThreadListLayout::Flat
    } else {
        ThreadListLayout::NestedByDate
    };
    let rollout_files = match list::discover_rollout_files(root.as_path(), layout).await {
        Ok(files) => files,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    let Some(plain_matches) = ripgrep_rollout_matches(
        rg_command,
        root.as_path(),
        &rollout_files,
        json_search_term.as_str(),
        search_term,
    )
    .await?
    else {
        return scan_rollout_matches(&rollout_files, json_search_term.as_str(), search_term).await;
    };
    let mut matches = plain_matches;
    matches.extend(scan_compressed_rollout_matches(&rollout_files, search_term).await?);
    Ok(matches)
}

async fn ripgrep_rollout_matches(
    rg_command: &Path,
    root: &Path,
    rollout_files: &[compression::RolloutFile],
    json_search_term: &str,
    search_term: &str,
) -> io::Result<Option<RolloutSearchMatches>> {
    let plain_paths = rollout_files
        .iter()
        .filter(|rollout_file| !rollout_file.is_compressed())
        .map(compression::RolloutFile::path)
        .collect::<Vec<_>>();
    if plain_paths.is_empty() {
        return Ok(Some(HashMap::new()));
    }

    let search_term = case_insensitive_literal_regex(search_term)?;
    let mut matches = HashMap::new();
    for paths in plain_paths.chunks(RIPGREP_PATH_BATCH_SIZE) {
        let output = match Command::new(rg_command)
            .arg("--json")
            .arg("--fixed-strings")
            .arg("--ignore-case")
            .arg("--no-ignore")
            .arg("--")
            .arg(json_search_term)
            .args(paths)
            .output()
            .await
        {
            Ok(output) => output,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        if !output.status.success() {
            if output.status.code() == Some(1) && output.stderr.is_empty() {
                continue;
            }

            return Err(io::Error::other(format!(
                "ripgrep rollout search failed under {}",
                root.display()
            )));
        }

        for line in String::from_utf8_lossy(output.stdout.as_slice()).lines() {
            let Some((path, snippet)) = parse_ripgrep_rollout_match(line, root, &search_term)
            else {
                continue;
            };
            matches.entry(path).or_insert(Some(snippet));
        }
    }

    Ok(Some(matches))
}

fn parse_ripgrep_rollout_match(
    line: &str,
    root: &Path,
    search_term: &Regex,
) -> Option<(PathBuf, String)> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type")?.as_str()? != "match" {
        return None;
    }
    let data = value.get("data")?;
    let path_text = data.get("path")?.get("text")?.as_str()?;
    let jsonl_line = data.get("lines")?.get("text")?.as_str()?;
    let path = PathBuf::from(path_text);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let snippet = content_match_snippet(jsonl_line, search_term)?;
    Some((path, snippet))
}

async fn scan_rollout_matches(
    rollout_files: &[compression::RolloutFile],
    json_search_term: &str,
    search_term: &str,
) -> io::Result<RolloutSearchMatches> {
    let mut matches = HashMap::new();
    let json_search_term = case_insensitive_literal_regex(json_search_term)?;

    for rollout_file in rollout_files {
        if rollout_file.is_compressed() {
            if let Some(snippet) =
                first_rollout_content_match_snippet(rollout_file.path(), search_term).await?
            {
                matches.insert(
                    compression::plain_rollout_path(rollout_file.path()),
                    Some(snippet),
                );
            }
            continue;
        }
        if rollout_contains(rollout_file.path(), &json_search_term).await? {
            matches.insert(rollout_file.path().to_path_buf(), None);
        }
    }

    Ok(matches)
}

async fn rollout_contains(path: &Path, search_term: &Regex) -> io::Result<bool> {
    let mut lines = compression::open_rollout_line_reader(path).await?;
    while let Some(line) = lines.next_line().await? {
        if search_term.is_match(line.as_str()) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub async fn first_rollout_content_match_snippet(
    path: &Path,
    search_term: &str,
) -> io::Result<Option<String>> {
    let mut lines = compression::open_rollout_line_reader(path).await?;
    let json_search_term = case_insensitive_literal_regex(json_escaped_search_term(search_term)?)?;
    let search_term = case_insensitive_literal_regex(search_term)?;
    while let Some(line) = lines.next_line().await? {
        if json_search_term.is_match(line.as_str())
            && let Some(snippet) = content_match_snippet(line.as_str(), &search_term)
        {
            return Ok(Some(snippet));
        }
    }
    Ok(None)
}

async fn scan_compressed_rollout_matches(
    rollout_files: &[compression::RolloutFile],
    search_term: &str,
) -> io::Result<RolloutSearchMatches> {
    let mut matches = HashMap::new();

    for rollout_file in rollout_files {
        if !rollout_file.is_compressed() {
            continue;
        }
        if let Some(snippet) =
            first_rollout_content_match_snippet(rollout_file.path(), search_term).await?
        {
            matches.insert(
                compression::plain_rollout_path(rollout_file.path()),
                Some(snippet),
            );
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

fn conversation_text_from_item(item: &RolloutItem) -> Option<String> {
    match item {
        RolloutItem::EventMsg(EventMsg::UserMessage(user)) => {
            let text = strip_user_message_prefix(user.message.as_str());
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        RolloutItem::EventMsg(EventMsg::AgentMessage(agent)) => {
            if agent.message.trim().is_empty() {
                None
            } else {
                Some(agent.message.trim().to_string())
            }
        }
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

fn content_item_text(item: &ContentItem) -> Option<&str> {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text.as_str()),
        ContentItem::InputImage { .. } => None,
    }
}

fn strip_user_message_prefix(text: &str) -> &str {
    match text.find(USER_MESSAGE_BEGIN) {
        Some(idx) => text[idx + USER_MESSAGE_BEGIN.len()..].trim(),
        None => text.trim(),
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
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::RolloutLine;
    use codex_protocol::protocol::UserMessageEvent;
    use serde_json::json;

    #[test]
    fn parses_ripgrep_json_match_with_content_snippet() {
        let rollout_line = RolloutLine {
            timestamp: "2026-07-09T00:00:00Z".to_string(),
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: format!("{USER_MESSAGE_BEGIN}\nplease find the needle in this rollout"),
                ..Default::default()
            })),
        };
        let jsonl_line = serde_json::to_string(&rollout_line).expect("serialize rollout line");
        let rg_line = json!({
            "type": "match",
            "data": {
                "path": { "text": "2026/07/09/rollout-test.jsonl" },
                "lines": { "text": jsonl_line }
            }
        })
        .to_string();
        let search_term = case_insensitive_literal_regex("needle").expect("regex");

        let (path, snippet) =
            parse_ripgrep_rollout_match(&rg_line, Path::new("/tmp/sessions"), &search_term)
                .expect("match should parse");

        assert_eq!(
            path,
            PathBuf::from("/tmp/sessions/2026/07/09/rollout-test.jsonl")
        );
        assert!(snippet.contains("needle"));
    }

    #[test]
    fn ignores_non_match_ripgrep_json_events() {
        let line = json!({
            "type": "begin",
            "data": {
                "path": { "text": "rollout-test.jsonl" }
            }
        })
        .to_string();
        let search_term = case_insensitive_literal_regex("needle").expect("regex");

        assert!(
            parse_ripgrep_rollout_match(&line, Path::new("/tmp/sessions"), &search_term).is_none()
        );
    }
}
