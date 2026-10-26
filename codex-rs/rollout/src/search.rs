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

/// Search matches keyed by the canonical `.jsonl` path for each rollout.
pub type RolloutSearchMatches = HashMap<PathBuf, Option<String>>;

/// Compiled literal matchers shared by the initial scan and snippet fallback.
pub struct RolloutSearchQuery {
    json_search_term: String,
    json_matcher: Regex,
    text_matcher: Regex,
}

impl RolloutSearchQuery {
    pub fn new(search_term: &str) -> io::Result<Self> {
        let json_search_term = json_escaped_search_term(search_term)?;
        Ok(Self {
            json_matcher: case_insensitive_literal_regex(&json_search_term)?,
            text_matcher: case_insensitive_literal_regex(search_term)?,
            json_search_term,
        })
    }

    pub async fn search_matches(
        &self,
        rg_command: &Path,
        codex_home: &Path,
        archived: bool,
    ) -> io::Result<RolloutSearchMatches> {
        search_rollout_matches_with_query(rg_command, codex_home, archived, self).await
    }

    pub async fn first_content_match_snippet(&self, path: &Path) -> io::Result<Option<String>> {
        let (_, snippet) =
            inspect_rollout_match(path, &self.json_matcher, &self.text_matcher).await?;
        Ok(snippet)
    }
}

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
    RolloutSearchQuery::new(search_term)?
        .search_matches(rg_command, codex_home, archived)
        .await
}

async fn search_rollout_matches_with_query(
    rg_command: &Path,
    codex_home: &Path,
    archived: bool,
    query: &RolloutSearchQuery,
) -> io::Result<RolloutSearchMatches> {
    let home = std::path::absolute(codex_home)?;
    let root = home.join(if archived {
        ARCHIVED_SESSIONS_SUBDIR
    } else {
        SESSIONS_SUBDIR
    });
    // Nearly every rollout starts with a payload reference (session metadata
    // carries the base instructions), and ripgrep cannot see the body behind
    // it. Search the retained bodies first so only references to matching
    // bodies become hydration candidates, rather than every rollout.
    let Some(payload_patterns) = matching_payload_patterns(
        rg_command,
        &home.join(super::payload_artifact::PAYLOAD_DIRECTORY),
        &query.json_search_term,
    )
    .await?
    else {
        return scan_rollout_matches(root.as_path(), query).await;
    };
    let Some(plain_matches) = ripgrep_rollout_paths(
        rg_command,
        root.as_path(),
        &query.json_search_term,
        &payload_patterns,
        &query.text_matcher,
    )
    .await?
    else {
        return scan_rollout_matches(root.as_path(), query).await;
    };
    let mut matches = HashMap::new();
    for (path, snippet) in plain_matches {
        if snippet.is_some() {
            insert_rollout_match(&mut matches, path, snippet);
            continue;
        }
        // A payload reference is only a candidate. Search its canonical hydrated lines
        // after ripgrep has exited, so storage cannot block draining the child pipes.
        let (matched, snippet) =
            inspect_rollout_match(&path, &query.json_matcher, &query.text_matcher).await?;
        if matched {
            insert_rollout_match(&mut matches, path, snippet);
        }
    }
    for (path, snippet) in scan_compressed_rollout_matches(root.as_path(), query).await? {
        insert_rollout_match(&mut matches, path, snippet);
    }
    Ok(matches)
}

/// Above this many matching bodies, every payload reference stays a candidate
/// instead of one literal per digest, bounding the ripgrep command line.
const MAX_PAYLOAD_DIGEST_PATTERNS: usize = 128;

/// Literal patterns naming rollout lines whose retained payload body contains
/// the search term: the digests of matching bodies, or the payload kind itself
/// when there are too many to name or the body search cannot complete. `None`
/// when ripgrep cannot run, so the caller keeps its directory-scan fallback.
async fn matching_payload_patterns(
    rg_command: &Path,
    payload_root: &Path,
    json_search_term: &str,
) -> io::Result<Option<Vec<String>>> {
    if !tokio::fs::try_exists(payload_root).await.unwrap_or(false) {
        return Ok(Some(Vec::new()));
    }
    let mut command = Command::new(rg_command);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    command
        .arg("--files-with-matches")
        .arg("--fixed-strings")
        .arg("--ignore-case")
        .arg("--no-ignore")
        .arg("--glob")
        .arg("*.json")
        .arg("-e")
        .arg(json_search_term)
        .arg("--")
        .arg(payload_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
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
        .ok_or_else(|| io::Error::other("ripgrep payload search stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("ripgrep payload search stderr was not captured"))?;
    let read_paths = async {
        let mut stdout = BufReader::new(stdout);
        let mut digests = HashSet::new();
        let mut line = Vec::new();
        while stdout.read_until(b'\n', &mut line).await? != 0 {
            if digests.len() <= MAX_PAYLOAD_DIGEST_PATTERNS {
                digests.extend(payload_digests_from_paths(&line));
            }
            // Once 129 distinct valid digests require the conservative fallback,
            // only drain/reap the child; never retain the remaining path list.
            line.clear();
        }
        let mut digests = digests.into_iter().collect::<Vec<_>>();
        digests.sort_unstable();
        Ok::<_, io::Error>(digests)
    };
    let drain_stderr = drain_and_check_empty(stderr);
    let wait_for_exit = child.wait();
    let (digests, stderr_is_empty, status) =
        tokio::try_join!(read_paths, drain_stderr, wait_for_exit)?;
    let all_references = vec![super::payload_artifact::KIND.to_string()];
    if !status.success() && !(status.code() == Some(1) && stderr_is_empty) {
        // An unreadable body must not hide a match: keep every reference.
        tracing::warn!(
            root = %payload_root.display(),
            "rollout payload search failed; inspecting every payload reference"
        );
        return Ok(Some(all_references));
    }
    Ok(Some(if digests.len() > MAX_PAYLOAD_DIGEST_PATTERNS {
        all_references
    } else {
        digests
    }))
}

/// Digests named by `--files-with-matches` output, one `<sha256>.json` per line.
fn payload_digests_from_paths(output: &[u8]) -> Vec<String> {
    let mut digests = output
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let name = line.rsplit(|byte| *byte == b'/' || *byte == b'\\').next()?;
            let digest = name.strip_suffix(b".json")?;
            (digest.len() == 64 && digest.iter().all(u8::is_ascii_hexdigit))
                .then(|| String::from_utf8_lossy(digest).to_ascii_lowercase())
        })
        .collect::<Vec<_>>();
    digests.sort_unstable();
    digests.dedup();
    digests
}

async fn ripgrep_rollout_paths(
    rg_command: &Path,
    root: &Path,
    json_search_term: &str,
    payload_patterns: &[String],
    search_term: &Regex,
) -> io::Result<Option<RolloutSearchMatches>> {
    if !tokio::fs::try_exists(root).await.unwrap_or(false) {
        return Ok(Some(HashMap::new()));
    }

    let mut command =
        rollout_ripgrep_command(rg_command, root, json_search_term, payload_patterns);
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
    let read_matches = read_ripgrep_rollout_matches(stdout, root, search_term);
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

    Ok(Some(matches))
}

fn rollout_ripgrep_command(
    rg_command: &Path,
    root: &Path,
    json_search_term: &str,
    payload_patterns: &[String],
) -> Command {
    let mut command = Command::new(rg_command);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    command
        .arg("--json")
        .arg("--max-count=1")
        .arg("--fixed-strings")
        .arg("--ignore-case")
        .arg("--no-ignore")
        .arg("--glob")
        .arg("*.jsonl")
        .arg("-e")
        .arg(json_search_term);
    // A reference to a matching body is a candidate; other references are not.
    for pattern in payload_patterns {
        command.arg("-e").arg(pattern);
    }
    command
        .arg("--")
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
    let path = compression::RolloutFile::from_path(path)?.into_path();
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
    query: &RolloutSearchQuery,
) -> io::Result<RolloutSearchMatches> {
    let mut matches = HashMap::new();
    let mut dirs = vec![root.to_path_buf()];

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
            let (matched, snippet) = inspect_rollout_match(
                rollout_file.path(),
                &query.json_matcher,
                &query.text_matcher,
            )
            .await?;
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
    let mut lines = compression::open_rollout_line_reader(path).await?;
    let mut matched = false;
    while let Some(line) = lines.next_line().await? {
        if !json_search_term.is_match(line.as_str()) {
            continue;
        }
        matched = true;
        if let Some(snippet) = content_match_snippet(line.as_str(), search_term) {
            return Ok((true, Some(snippet)));
        }
    }
    Ok((matched, None))
}

pub async fn first_rollout_content_match_snippet(
    path: &Path,
    search_term: &str,
) -> io::Result<Option<String>> {
    RolloutSearchQuery::new(search_term)?
        .first_content_match_snippet(path)
        .await
}

async fn scan_compressed_rollout_matches(
    root: &Path,
    query: &RolloutSearchQuery,
) -> io::Result<RolloutSearchMatches> {
    let mut matches = HashMap::new();
    let mut dirs = vec![root.to_path_buf()];

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
            let (matched, snippet) = inspect_rollout_match(
                rollout_file.path(),
                &query.json_matcher,
                &query.text_matcher,
            )
            .await?;
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
    let mut serialized = serde_json::to_string(search_term).map_err(io::Error::other)?;
    serialized.pop();
    serialized.remove(0);
    Ok(serialized)
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
        | RolloutItem::ToolManifest(_)
        | RolloutItem::SamplingBoundary(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::EventMsg(_)
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::AdditionalContextState { .. } => None,
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
        ContentItem::InputText { text }
        | ContentItem::OutputText { text }
        | ContentItem::Refusal { refusal: text } => Some(text.as_str()),
        ContentItem::InputImage { .. } => None,
    }
}

fn excerpt_around_match(text: &str, search_term: &Regex) -> Option<String> {
    let matched = search_term.find(text)?;
    let match_start = matched.start();
    let match_end = matched.end();
    let excerpt_start = char_start_before(text, match_start, MATCH_CONTEXT_BEFORE_CHARS);
    let excerpt_end = char_end_after(text, match_end, MATCH_CONTEXT_AFTER_CHARS);
    let excerpt = normalize_preview_text(&text[excerpt_start..excerpt_end]);
    if excerpt.is_empty() {
        return None;
    }

    let mut snippet = String::new();
    if excerpt_start > 0 {
        snippet.push_str("... ");
    }
    snippet.push_str(&excerpt);
    if excerpt_end < text.len() {
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
        .take(chars_before)
        .last()
        .map_or(byte_index, |(idx, _)| idx)
}

fn char_end_after(text: &str, byte_index: usize, chars_after: usize) -> usize {
    text[byte_index..]
        .char_indices()
        .nth(chars_after)
        .map(|(offset, _)| byte_index.saturating_add(offset))
        .unwrap_or(text.len())
}

