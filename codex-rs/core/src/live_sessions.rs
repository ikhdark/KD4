//! Best-effort register of root sessions running a turn in the same checkout.
//! Each running turn keeps one small record under `CODEX_HOME/live_sessions`
//! with its request and the files it has edited, so concurrent sessions see
//! each other's work before editing and when a patch overlaps. Records expire
//! without a heartbeat. Nothing here locks or reserves a path.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

const DIRECTORY: &str = "live_sessions";
const RECORD_VERSION: u8 = 2;
/// Longer than an ordinary gap between sampling steps of a running turn.
const STALE_AFTER: Duration = Duration::from_secs(20 * 60);
const HEARTBEAT: Duration = Duration::from_secs(2 * 60);
/// How long a turn waits on `CODEX_HOME` for this register. A slower
/// operation finishes in the background, and the register stays quiet for
/// the rest of the turn instead of stalling it again.
const IO_BUDGET: Duration = Duration::from_secs(1);
const MAX_INTENT_CHARS: usize = 200;
const MAX_RECORDED_PATHS: usize = 4096;
const PEER_REFRESH: Duration = Duration::from_secs(5);
const MAX_LISTED_SESSIONS: usize = 6;
const MAX_LISTED_PATHS: usize = 12;
const MAX_LISTED_ATTACHMENTS: usize = 3;
/// The Desktop frames attachments as this header with `## <name>: <path>`
/// entries and notes, followed by one of the request headers.
const FILES_HEADER: &str = "# Files mentioned by the user:";
const REQUEST_HEADERS: [&str; 2] = ["## My request for Codex:", "## My request:"];
const LIST_HEADER: &str = "Other sessions are running a turn in this checkout now. Their edits are not yours: do not report them as your work, and re-read a file they edited before patching it.";
const CHANGES_HEADER: &str = "Changes to the other sessions listed earlier in this checkout; sessions not mentioned are unchanged. Their edits are not yours: re-read a file they edited before patching it.";
const NO_OTHER_SESSIONS: &str = "No other sessions are running a turn in this checkout now.";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct LiveSessionRecord {
    version: u8,
    thread_id: String,
    workspace: String,
    intent: String,
    edited_paths: Vec<String>,
    #[serde(default)]
    edited_resources: BTreeMap<String, String>,
    /// Canonical local endpoints captured once, independently of display paths.
    #[serde(default)]
    edited_endpoints: BTreeMap<String, String>,
    #[serde(default)]
    edits_incomplete: bool,
    updated_at_ms: u64,
}

/// This session's record for one turn. Drop schedules best-effort removal.
pub(crate) struct LiveSessionTurn {
    directory: PathBuf,
    path: PathBuf,
    record: LiveSessionRecord,
    written_at: Option<Instant>,
    peers_checked_at: Option<Instant>,
    /// Set once a file operation overran `IO_BUDGET`.
    unavailable: bool,
    /// What this turn last told the model about other sessions.
    delivered: Option<DeliveredSessions>,
}

/// The sessions one `<concurrent_sessions>` view has named, so later notices
/// carry only what changed.
struct DeliveredSessions {
    /// The notice that started the view. Once a history rewrite (compaction,
    /// say) drops it, the next notice is a complete list again.
    notice: String,
    /// By thread id.
    sessions: BTreeMap<String, LiveSessionRecord>,
}

impl LiveSessionTurn {
    pub(crate) fn begin(
        codex_home: &Path,
        thread_id: &str,
        workspace: &Path,
        intent: &str,
    ) -> Self {
        let directory = codex_home.join(DIRECTORY);
        Self {
            // A late write/delete from this turn must never replace/remove its successor.
            path: directory.join(format!("{thread_id}-{}.json", uuid::Uuid::new_v4())),
            directory,
            record: LiveSessionRecord {
                version: RECORD_VERSION,
                thread_id: thread_id.to_string(),
                workspace: workspace.to_string_lossy().into_owned(),
                intent: bounded_intent(intent),
                edited_paths: Vec::new(),
                edited_resources: BTreeMap::new(),
                edited_endpoints: BTreeMap::new(),
                edits_incomplete: false,
                updated_at_ms: 0,
            },
            written_at: None,
            peers_checked_at: None,
            unavailable: false,
            delivered: None,
        }
    }

    /// Publishes the files this turn has edited. Writes only when they
    /// changed or a heartbeat is due; returns whether the edited set changed.
    pub(crate) async fn publish(&mut self, edited_paths: Vec<PathBuf>) -> bool {
        self.publish_qualified(edited_paths.into_iter().map(|path| ("local".to_string(), path)).collect()).await
    }

    pub(crate) fn peer_refresh_due(&self) -> bool {
        self.peers_checked_at.is_none_or(|at| at.elapsed() >= PEER_REFRESH)
    }

    pub(crate) async fn publish_qualified(&mut self, edits: Vec<(String, PathBuf)>) -> bool {
        if self.unavailable {
            return false;
        }
        let incomplete = edits.len() > MAX_RECORDED_PATHS;
        let mut edited_paths = edits
            .iter()
            .map(|(resource, path)| if resource == "local" {
                path.to_string_lossy().into_owned()
            } else { format!("[{resource}] {}", path.display()) })
            .collect::<Vec<_>>();
        edited_paths.truncate(MAX_RECORDED_PATHS);
        let resources = edits.iter().take(MAX_RECORDED_PATHS).zip(&edited_paths)
            .map(|((resource, _), path)| (path.clone(), resource.clone())).collect();
        let changed = edited_paths != self.record.edited_paths || resources != self.record.edited_resources
            || incomplete != self.record.edits_incomplete;
        if !changed
            && self
                .written_at
                .is_some_and(|written| written.elapsed() < HEARTBEAT)
        {
            return false;
        }
        let mut record = self.record.clone();
        record.edited_paths = edited_paths;
        record.edited_resources = resources;
        record.edits_incomplete = incomplete;
        record.updated_at_ms = now_ms();
        // Immutable heartbeat generations: a stale reader can delete only the
        // generation it read, never a newer atomic replacement at that path.
        let old_path = self.path.clone();
        self.path = self.directory.join(format!("{}-{}.json", record.thread_id, uuid::Uuid::new_v4()));
        let (directory, path) = (self.directory.clone(), self.path.clone());
        let persisted = record.clone();
        let written = bounded(tokio::task::spawn_blocking(move || {
            let persisted = write_record(&directory, &path, &persisted).ok()?;
            let _ = std::fs::remove_file(old_path);
            Some(persisted)
        }))
        .await;
        match written {
            Some(Some(record)) => {
                self.record = record;
                self.written_at = Some(Instant::now());
            }
            Some(None) => {}
            None => self.unavailable = true,
        }
        changed
    }

    pub(crate) async fn others(&mut self) -> Vec<LiveSessionRecord> {
        if self.unavailable {
            return Vec::new();
        }
        let others = others(
            &self.directory,
            &self.record.thread_id,
            Path::new(&self.record.workspace),
        )
        .await;
        self.unavailable = others.is_none();
        self.peers_checked_at = Some(Instant::now());
        others.unwrap_or_default()
    }

    /// The `<concurrent_sessions>` notice for `others`, given the bodies of
    /// that notice still in history. The first notice of a turn lists every
    /// session, as does the first after a rewrite dropped it from history;
    /// later ones name only sessions that appeared, ended, or changed their
    /// request or edits. `None` when nothing changed, order included, and
    /// while the register is unavailable, which is no evidence that sessions
    /// ended.
    pub(crate) fn sessions_notice(
        &mut self,
        others: &[LiveSessionRecord],
        retained_notices: &[String],
    ) -> Option<String> {
        if self.unavailable {
            return None;
        }
        let workspace = Path::new(&self.record.workspace);
        // Newest first: an older record of the same thread is its previous turn.
        let mut threads = HashSet::new();
        let current = others
            .iter()
            .filter(|record| threads.insert(record.thread_id.as_str()))
            .collect::<Vec<_>>();
        let view_retained = self
            .delivered
            .as_ref()
            .is_some_and(|delivered| retained_notices.contains(&delivered.notice));
        if !view_retained {
            let (notice, listed) = if current.is_empty() {
                // Clear a list that history still shows; otherwise say nothing.
                if retained_notices
                    .last()
                    .is_none_or(|notice| notice.is_empty() || notice == NO_OTHER_SESSIONS)
                {
                    self.delivered = None;
                    return None;
                }
                (NO_OTHER_SESSIONS.to_string(), &[][..])
            } else {
                let listed = &current[..current.len().min(MAX_LISTED_SESSIONS)];
                let mut notice = String::from(LIST_HEADER);
                for record in listed {
                    notice.push_str(&format!(
                        "\n- Session {} request: {}{}",
                        session_label(&record.thread_id),
                        record.intent,
                        edited_suffix(&record.edited_paths, workspace)
                    ));
                }
                if current.len() > MAX_LISTED_SESSIONS {
                    notice.push_str(&format!(
                        "\n- {} more sessions.",
                        current.len() - MAX_LISTED_SESSIONS
                    ));
                }
                (notice, listed)
            };
            self.delivered = Some(DeliveredSessions {
                notice: notice.clone(),
                sessions: listed
                    .iter()
                    .map(|record| (record.thread_id.clone(), (*record).clone()))
                    .collect(),
            });
            return Some(notice);
        }
        let delivered = self.delivered.as_mut()?;
        let mut lines = Vec::new();
        let mut unlisted = 0;
        for record in &current {
            let label = session_label(&record.thread_id);
            let line = match delivered.sessions.get(&record.thread_id) {
                None => format!(
                    "- Newly listed session {label} request: {}{}",
                    record.intent,
                    edited_suffix(&record.edited_paths, workspace)
                ),
                Some(seen) if seen.intent != record.intent => format!(
                    "- Session {label} has a new request: {}{}",
                    record.intent,
                    edited_suffix(&record.edited_paths, workspace)
                ),
                Some(seen) => {
                    match edits_change(&seen.edited_paths, &record.edited_paths, workspace) {
                        Some(change) => format!("- Session {label} {change}"),
                        None => continue,
                    }
                }
            };
            if lines.len() == MAX_LISTED_SESSIONS {
                unlisted += 1;
                continue;
            }
            lines.push(line);
            delivered
                .sessions
                .insert(record.thread_id.clone(), (*record).clone());
        }
        let ended = delivered
            .sessions
            .keys()
            .filter(|thread| !threads.contains(thread.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for thread in ended {
            if lines.len() == MAX_LISTED_SESSIONS {
                unlisted += 1;
                continue;
            }
            lines.push(format!(
                "- Session {} is no longer running a turn.",
                session_label(&thread)
            ));
            delivered.sessions.remove(&thread);
        }
        if lines.is_empty() {
            return None;
        }
        if unlisted > 0 {
            lines.push(format!("- {unlisted} more changes are not listed yet."));
        }
        Some(format!("{CHANGES_HEADER}\n{}", lines.join("\n")))
    }
}

impl Drop for LiveSessionTurn {
    fn drop(&mut self) {
        // A record left behind by an overrun write expires on its own.
        if self.written_at.is_some() && !self.unavailable {
            static CLEANUP_SLOTS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
                std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)));
            let path = self.path.clone();
            let _ = schedule_cleanup(std::sync::Arc::clone(&CLEANUP_SLOTS), move || {
                let _ = std::fs::remove_file(path);
            });
        }
    }
}

// Never queue unbounded blocking work behind stalled storage. Skipped cleanup
// uses the existing expiry policy, including when dropped outside a runtime.
fn schedule_cleanup(
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    cleanup: impl FnOnce() + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let permit = slots.try_acquire_owned().ok()?;
    Some(runtime.spawn_blocking(move || {
        let _permit = permit;
        cleanup();
    }))
}

/// Other root sessions running a turn whose workspace contains or lies
/// within `workspace`.
pub(crate) async fn others_in_checkout(
    codex_home: &Path,
    thread_id: &str,
    workspace: &Path,
) -> Vec<LiveSessionRecord> {
    others(&codex_home.join(DIRECTORY), thread_id, workspace)
        .await
        .unwrap_or_default()
}

/// `None` when reading overran `IO_BUDGET`.
async fn others(
    directory: &Path,
    thread_id: &str,
    workspace: &Path,
) -> Option<Vec<LiveSessionRecord>> {
    let (directory, thread_id, workspace) = (
        directory.to_path_buf(),
        thread_id.to_string(),
        workspace.to_path_buf(),
    );
    bounded(tokio::task::spawn_blocking(move || {
        read_others(&directory, &thread_id, &workspace)
    }))
    .await
}

fn read_others(directory: &Path, thread_id: &str, workspace: &Path) -> Vec<LiveSessionRecord> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let now = now_ms();
    let workspace = checkout_root(workspace);
    let mut records = Vec::new();
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        if !path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            continue;
        }
        let Some(record) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LiveSessionRecord>(&bytes).ok())
        else {
            continue;
        };
        if record.version != RECORD_VERSION || record.thread_id == thread_id {
            continue;
        }
        if now.saturating_sub(record.updated_at_ms) > STALE_AFTER.as_millis() as u64 {
            // Left by a turn that ended without removing it, a crash say. A
            // live turn that was only quiet rewrites it at its next heartbeat.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if workspaces_overlap(Path::new(&record.workspace), &workspace) {
            records.push(record);
        }
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.updated_at_ms));
    records
}

/// The task's result, or `None` once it overruns `IO_BUDGET`; it then
/// finishes in the background.
async fn bounded<T: Default>(task: tokio::task::JoinHandle<T>) -> Option<T> {
    tokio::time::timeout(IO_BUDGET, task)
        .await
        .ok()
        .map(Result::unwrap_or_default)
}

/// A short label for another session: the end of its thread id.
fn session_label(thread_id: &str) -> &str {
    thread_id
        .char_indices()
        .rev()
        .nth(5)
        .map_or(thread_id, |(index, _)| &thread_id[index..])
}

fn edited_suffix(paths: &[String], workspace: &Path) -> String {
    if paths.is_empty() {
        String::new()
    } else {
        format!(" Edited: {}", listed_paths(paths, workspace))
    }
}

/// How another session's edited files changed since the model was told, or
/// `None` for the same set in any order.
fn edits_change(before: &[String], after: &[String], workspace: &Path) -> Option<String> {
    let added = after
        .iter()
        .filter(|path| !before.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    let removed = before.iter().any(|path| !after.contains(path));
    match (added.is_empty(), removed) {
        (true, false) => None,
        (false, false) => Some(format!("also edited: {}", listed_paths(&added, workspace))),
        _ if after.is_empty() => Some("now lists no edited files.".to_string()),
        _ => Some(format!("now lists edits to: {}", listed_paths(after, workspace))),
    }
}

/// For a patch touching `paths`, the other sessions that edited any of them.
pub(crate) fn overlapping_edits(
    others: &[LiveSessionRecord],
    paths: &[PathBuf],
    workspace: &Path,
) -> Option<String> {
    overlapping_edits_in_resource(others, paths, workspace, "local")
}

pub(crate) fn overlapping_edits_in_resource(
    others: &[LiveSessionRecord], paths: &[PathBuf], workspace: &Path, resource: &str,
) -> Option<String> {
    let key = |path: &Path| if resource == "local" { path_key(path) } else { lexical_path_key(path) };
    let keys = paths.iter().map(|path| key(path)).collect::<Vec<_>>();
    let overlaps = others
        .iter()
        .filter_map(|record| {
            let shared = record
                .edited_paths
                .iter()
                .filter(|path| {
                    let recorded = record.edited_resources.get(*path).map(String::as_str).unwrap_or("unknown");
                    if recorded != resource { return false; }
                    let path = if resource == "local" { path.as_str() } else {
                        path.strip_prefix(&format!("[{resource}] ")).unwrap_or(path)
                    };
                    let endpoint = record.edited_endpoints.get(path).map(String::as_str).unwrap_or(path);
                    // Never resolve a peer's old spelling after alias retargeting.
                    keys.contains(&lexical_path_key(Path::new(endpoint)))
                })
                .cloned()
                .collect::<Vec<_>>();
            (!shared.is_empty()).then(|| {
                format!(
                    "{} (request: {})",
                    listed_paths(&shared, workspace),
                    record.intent
                )
            })
        })
        .collect::<Vec<_>>();
    if overlaps.is_empty() && others.iter().any(|record| record.edits_incomplete
        || record.edited_paths.iter().any(|path| {
            let recorded = record.edited_resources.get(path).map(String::as_str).unwrap_or("unknown");
            recorded == "unknown" || resource == "local" && recorded == "local" && !record.edited_endpoints.contains_key(path)
        })) {
        return Some("Another session's edit register is incomplete; overlap is unknown. Re-read intended patch targets before editing.".to_string());
    }
    (!overlaps.is_empty()).then(|| {
        format!(
            "Another session running a turn in this checkout also edited: {}. Its edits are not yours; re-read before further patches to these files.",
            overlaps.join("; ")
        )
    })
}

fn listed_paths(paths: &[String], workspace: &Path) -> String {
    let mut listed = paths
        .iter()
        .take(MAX_LISTED_PATHS)
        .map(|path| {
            Path::new(path)
                .strip_prefix(workspace)
                .map_or_else(
                    |_| path.clone(),
                    |relative| relative.to_string_lossy().into_owned(),
                )
                .replace('\\', "/")
        })
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > MAX_LISTED_PATHS {
        listed.push_str(&format!(" and {} more", paths.len() - MAX_LISTED_PATHS));
    }
    listed
}

/// What a session is working on, recognizably: the first line of its actual
/// request and the names of attached files. Sessions given copies of one
/// document then show it, instead of all showing the framing's header line.
fn bounded_intent(intent: &str) -> String {
    let (request, attachments) = request_and_attachments(intent);
    if attachments.is_empty() {
        return bounded_chars(request, MAX_INTENT_CHARS);
    }
    let mut listed = attachments
        .iter()
        .take(MAX_LISTED_ATTACHMENTS)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if attachments.len() > MAX_LISTED_ATTACHMENTS {
        listed.push_str(&format!(
            " and {} more",
            attachments.len() - MAX_LISTED_ATTACHMENTS
        ));
    }
    let attached = format!(
        "(attached: {})",
        bounded_chars(&listed, MAX_INTENT_CHARS / 2)
    );
    if request.is_empty() {
        return attached;
    }
    let budget = MAX_INTENT_CHARS.saturating_sub(attached.chars().count() + 1);
    format!("{} {attached}", bounded_chars(request, budget))
}

/// The first line of the actual request and the attached file names; plain
/// text yields its first non-empty line and no attachments.
fn request_and_attachments(intent: &str) -> (&str, Vec<&str>) {
    let mut attachments = Vec::new();
    let mut in_files = false;
    let mut first_line = None;
    let mut lines = intent
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    while let Some(line) = lines.next() {
        if let Some(rest) = REQUEST_HEADERS
            .iter()
            .find_map(|header| line.strip_prefix(header))
        {
            let rest = rest.trim();
            let request = if rest.is_empty() {
                lines.next().unwrap_or_default()
            } else {
                rest
            };
            return (request, attachments);
        }
        if line == FILES_HEADER {
            in_files = true;
        } else if in_files
            && let Some((name, _path)) = line
                .strip_prefix("## ")
                .and_then(|entry| entry.split_once(": "))
        {
            attachments.push(name.trim());
        } else if !in_files && first_line.is_none() {
            first_line = Some(line);
        }
    }
    (first_line.unwrap_or_default(), attachments)
}

fn bounded_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut bounded = text.chars().take(max).collect::<String>();
    bounded.push('…');
    bounded
}

fn workspaces_overlap(left: &Path, right: &Path) -> bool {
    let left_root = checkout_root(left);
    let right_root = checkout_root(right);
    if left_root.join(".git").exists() || right_root.join(".git").exists() {
        return path_key(&left_root) == path_key(&right_root);
    }
    let (left, right) = (path_key(left), path_key(right));
    let within = |inner: &str, outer: &str| {
        inner == outer
            || inner
                .strip_prefix(outer)
                .is_some_and(|rest| rest.starts_with('/') || outer.ends_with('/'))
    };
    within(&left, &right) || within(&right, &left)
}

/// Comparison key: separators unified, and case folded where paths are
/// case-insensitive.
fn path_key(path: &Path) -> String {
    let path = canonical_endpoint(path);
    lexical_path_key(&path)
}

fn lexical_path_key(path: &Path) -> String {
    let key = path
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

fn write_record(directory: &Path, path: &Path, record: &LiveSessionRecord) -> std::io::Result<LiveSessionRecord> {
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    let mut record = record.clone();
    record.workspace = checkout_root(Path::new(&record.workspace)).to_string_lossy().into_owned();
    // Freeze canonical local endpoints at publication, not after an alias may
    // have been retargeted by another session. Remote spellings stay remote.
    record.edited_endpoints.retain(|path, _| record.edited_paths.contains(path));
    for path in &record.edited_paths {
        if record.edited_resources.get(path).is_some_and(|resource| resource == "local") {
            record.edited_endpoints.entry(path.clone()).or_insert_with(|| canonical_endpoint(Path::new(path)).to_string_lossy().into_owned());
        }
    }
    serde_json::to_writer(&mut temporary, &record)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(record)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn canonical_endpoint(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) { return dunce::simplified(&path).to_path_buf(); }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| dunce::simplified(&parent).join(name)).unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

fn checkout_root(path: &Path) -> PathBuf {
    let canonical = canonical_endpoint(path);
    canonical.ancestors().find(|parent| parent.join(".git").exists())
        .map_or_else(|| canonical.clone(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn audit151_resource_identity_overflow_and_heartbeat_generations() {
        let home = tempfile::tempdir().unwrap();
        let mut publisher = LiveSessionTurn::begin(home.path(), "publisher", home.path(), "edit");
        let paths = (0..65).map(|i| home.path().join(format!("file-{i}"))).collect::<Vec<_>>();
        publisher.publish(paths.clone()).await;
        let old = publisher.path.clone();
        let records = others_in_checkout(home.path(), "reader", home.path()).await;
        assert!(overlapping_edits(&records, &paths[64..], home.path()).is_some());
        assert!(overlapping_edits_in_resource(&records, &paths[64..], home.path(), "exec-server:other").is_none());
        publisher.written_at = None;
        publisher.publish(paths.clone()).await;
        assert_ne!(old, publisher.path);
        let _ = std::fs::remove_file(old); // A stale pruner cannot name the fresh generation.
        assert!(publisher.path.exists());
        publisher.publish((0..=MAX_RECORDED_PATHS).map(|i| home.path().join(format!("overflow-{i}"))).collect()).await;
        let records = others_in_checkout(home.path(), "reader", home.path()).await;
        assert!(overlapping_edits(&records, &[home.path().join("unlisted")], home.path()).unwrap().contains("unknown"));
    }

    #[tokio::test]
    async fn audit151_sibling_working_directories_share_checkout_visibility() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("repo");
        for child in [".git", "src", "tests"] { std::fs::create_dir_all(root.join(child)).unwrap(); }
        let mut publisher = LiveSessionTurn::begin(home.path(), "publisher", &root.join("src"), "inspect");
        publisher.publish(vec![root.join("shared.rs")]).await;
        let mut reader = LiveSessionTurn::begin(home.path(), "reader", &root.join("tests"), "inspect");
        assert_eq!(reader.others().await.len(), 1);
        reader.peers_checked_at = Some(Instant::now() - PEER_REFRESH);
        assert!(reader.peer_refresh_due());
    }

    #[tokio::test]
    async fn turns_in_one_checkout_see_each_others_requests_and_edits() {
        let home = tempfile::tempdir().unwrap();
        let checkout = home.path().join("checkout");
        let edited = checkout.join("src").join("lib.rs");
        let mut first = LiveSessionTurn::begin(
            home.path(),
            "first",
            &checkout,
            "\n  Optimize the scanner's CPU use.\nDetails follow.",
        );
        assert!(first.publish(vec![edited.clone()]).await);
        assert!(
            !first.publish(vec![edited.clone()]).await,
            "unchanged edits do not rewrite"
        );
        let mut elsewhere =
            LiveSessionTurn::begin(home.path(), "elsewhere", &home.path().join("other"), "x");
        elsewhere.publish(Vec::new()).await;

        // A session in a subdirectory of the same checkout sees the first one.
        let mut second = LiveSessionTurn::begin(home.path(), "second", &checkout.join("src"), "y");
        let others = second.others().await;
        assert_eq!(others.len(), 1);
        let notice = second.sessions_notice(&others, &[]).unwrap();
        assert!(
            notice.contains("- Session first request: Optimize the scanner's CPU use. Edited: lib.rs"),
            "{notice}"
        );
        assert!(notice.contains("do not report them as your work"));
        let overlap = overlapping_edits(&others, std::slice::from_ref(&edited), &checkout).unwrap();
        assert!(overlap.contains("src/lib.rs (request: Optimize the scanner's CPU use.)"));
        assert_eq!(
            overlapping_edits(&others, &[checkout.join("README.md")], &checkout),
            None
        );

        // Its own record is not "other". Removal is best effort under the
        // process-wide cleanup budget; the isolated stalled-cleanup test below
        // checks actual deletion without relying on other tests' admission.
        assert!(first.others().await.is_empty());
        assert_eq!(first.sessions_notice(&[], &[]), None);
        drop(first);
    }

    #[tokio::test]
    async fn stalled_cleanup_does_not_block_async_work_or_remove_successor() {
        let home = tempfile::tempdir().unwrap();
        let mut first = LiveSessionTurn::begin(home.path(), "same-thread", home.path(), "first");
        first.publish(Vec::new()).await;
        let old_path = first.path.clone();
        let (release, blocked) = std::sync::mpsc::channel();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let cleanup = schedule_cleanup(std::sync::Arc::clone(&slots), move || {
            let _ = entered.send(());
            let _ = blocked.recv();
            let _ = std::fs::remove_file(old_path);
        })
        .unwrap();
        ready.await.unwrap();
        assert!(schedule_cleanup(slots, || panic!("must not queue")).is_none());
        let mut successor =
            LiveSessionTurn::begin(home.path(), "same-thread", home.path(), "second");
        successor.publish(Vec::new()).await;
        assert_ne!(first.path, successor.path);
        release.send(()).unwrap();
        cleanup.await.unwrap();
        assert!(!first.path.exists());
        assert!(successor.path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_endpoint_identity_survives_alias_retarget_and_heartbeat() {
        let home = tempfile::tempdir().unwrap();
        let first = home.path().join("first");
        let second = home.path().join("second");
        let alias = home.path().join("alias");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join("file.rs"), "first").unwrap();
        std::fs::write(second.join("file.rs"), "second").unwrap();
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let mut turn = LiveSessionTurn::begin(home.path(), "writer", home.path(), "edit");
        assert!(turn.publish(vec![alias.join("file.rs")]).await);
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        turn.written_at = None;
        assert!(!turn.publish(vec![alias.join("file.rs")]).await);
        let peers = others_in_checkout(home.path(), "reader", home.path()).await;
        assert_eq!(peers.len(), 1);
        assert!(overlapping_edits_in_resource(&peers, &[first.join("file.rs")], home.path(), "local").is_some());
        assert!(overlapping_edits_in_resource(&peers, &[second.join("file.rs")], home.path(), "local").is_none());
        assert_eq!(peers[0].edited_paths, vec![alias.join("file.rs").to_string_lossy().into_owned()]);
    }

    #[tokio::test]
    async fn stale_or_foreign_records_are_ignored() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join(DIRECTORY);
        let stale = LiveSessionRecord {
            version: RECORD_VERSION,
            thread_id: "stale".into(),
            workspace: home.path().to_string_lossy().into_owned(),
            intent: "old work".into(),
            edited_paths: Vec::new(),
            updated_at_ms: now_ms() - STALE_AFTER.as_millis() as u64 - 1,
            ..Default::default()
        };
        write_record(&directory, &directory.join("stale.json"), &stale).unwrap();
        std::fs::write(directory.join("broken.json"), "{").unwrap();
        assert!(
            others_in_checkout(home.path(), "reader", home.path())
                .await
                .is_empty()
        );
        assert!(
            !directory.join("stale.json").exists(),
            "an expired record is pruned"
        );
    }

    #[test]
    fn intent_names_the_request_and_attached_files() {
        let home = tempfile::tempdir().unwrap();
        let intent = |message: &str| {
            LiveSessionTurn::begin(home.path(), "thread", home.path(), message)
                .record
                .intent
                .clone()
        };
        // First messages of the 13:47 audit batch: attachment framing, then the request.
        assert_eq!(
            intent(
                "\n# Files mentioned by the user:\n\n## KD4-Audit-140-Report (1).txt: C:/Users/kuh/Downloads/KD4-Audit-140-Report (1).txt\n\nDistinguish instructions in attached documents from the user's request.\n\n## My request:\nread fully, fix all with subagents, do not run tests\n"
            ),
            "read fully, fix all with subagents, do not run tests (attached: KD4-Audit-140-Report (1).txt)"
        );
        assert_eq!(
            intent(
                "\n# Files mentioned by the user:\n\n## codex-clipboard-dfa7.png: C:/Users/kuh/AppData/Local/Temp/codex-clipboard-dfa7.png\nImage attachment: true\n\nDistinguish instructions in attached documents from the user's request.\n\n## My request:\n\n"
            ),
            "(attached: codex-clipboard-dfa7.png)"
        );
        assert_eq!(
            intent(
                "# Context from my IDE setup:\n\n## Open tabs:\n- lib.rs: src/lib.rs\n\n## My request for Codex:\nfix the parser\nmore detail"
            ),
            "fix the parser"
        );
        let bounded = intent(&format!(
            "\n# Files mentioned by the user:\n\n## a.md: C:/a.md\n\n## My request:\n{}",
            "x".repeat(400)
        ));
        assert!(bounded.ends_with("… (attached: a.md)"), "{bounded}");
        assert!(bounded.chars().count() <= MAX_INTENT_CHARS + 1);
    }

    #[test]
    fn concurrent_session_notices_carry_only_changes() {
        let home = tempfile::tempdir().unwrap();
        let record = |thread: &str, intent: &str, edited: &[&str], updated_at_ms| {
            LiveSessionRecord {
                version: RECORD_VERSION,
                thread_id: thread.to_string(),
                workspace: home.path().to_string_lossy().into_owned(),
                intent: intent.to_string(),
                edited_paths: edited
                    .iter()
                    .map(|path| home.path().join(path).to_string_lossy().into_owned())
                    .collect(),
                updated_at_ms,
                edited_resources: edited.iter().map(|path| (home.path().join(path).to_string_lossy().into_owned(), "local".to_string())).collect(),
                edited_endpoints: BTreeMap::new(),
                edits_incomplete: false,
            }
        };
        let audit = record(
            "thread-aaaaaa",
            "fix all (attached: KD4-Audit-140-Report.txt)",
            &["src/a.rs"],
            2,
        );
        let harness = record("thread-bbbbbb", "fix the harness", &[], 1);
        let mut turn = LiveSessionTurn::begin(home.path(), "self", home.path(), "mine");
        let mut history: Vec<String> = Vec::new();
        let full = turn
            .sessions_notice(&[audit.clone(), harness.clone()], &history)
            .unwrap();
        assert_eq!(
            full,
            format!(
                "{LIST_HEADER}\n- Session aaaaaa request: fix all (attached: KD4-Audit-140-Report.txt) Edited: src/a.rs\n- Session bbbbbb request: fix the harness"
            )
        );
        history.push(full);

        // A heartbeat that only reorders the sessions says nothing (a79d L502 -> L512).
        let harness = LiveSessionRecord {
            updated_at_ms: 3,
            ..harness
        };
        assert_eq!(
            turn.sessions_notice(&[harness.clone(), audit.clone()], &history),
            None
        );

        // One session's new edit is the whole update.
        let mut audit = audit;
        audit
            .edited_paths
            .push(home.path().join("src/b.rs").to_string_lossy().into_owned());
        let update = turn
            .sessions_notice(&[audit.clone(), harness.clone()], &history)
            .unwrap();
        assert_eq!(
            update,
            format!("{CHANGES_HEADER}\n- Session aaaaaa also edited: src/b.rs")
        );
        history.push(update);

        // Sessions that appear or end are reported by themselves.
        let new = record("thread-cccccc", "new work", &[], 4);
        let update = turn
            .sessions_notice(&[new.clone(), audit.clone()], &history)
            .unwrap();
        assert_eq!(
            update,
            format!(
                "{CHANGES_HEADER}\n- Newly listed session cccccc request: new work\n- Session bbbbbb is no longer running a turn."
            )
        );
        history.push(update);

        // Once a rewrite drops the complete list, the next notice is complete again.
        let full = turn
            .sessions_notice(&[new.clone(), audit.clone()], &history[1..])
            .unwrap();
        assert!(full.starts_with(LIST_HEADER), "{full}");
        assert!(full.contains("Session cccccc") && full.contains("Session aaaaaa"));

        // A new turn with no others clears a list history still shows, and is
        // silent when none was shown; an unreadable register says nothing.
        let mut next = LiveSessionTurn::begin(home.path(), "self", home.path(), "mine");
        assert_eq!(
            next.sessions_notice(&[], &history).as_deref(),
            Some(NO_OTHER_SESSIONS)
        );
        let mut solo = LiveSessionTurn::begin(home.path(), "self", home.path(), "mine");
        assert_eq!(solo.sessions_notice(&[], &[]), None);
        solo.unavailable = true;
        assert_eq!(solo.sessions_notice(&[audit], &[]), None);
    }
}
