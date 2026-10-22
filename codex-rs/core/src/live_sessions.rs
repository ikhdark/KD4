//! Best-effort register of root sessions running a turn in the same checkout.
//! Each running turn keeps one small record under `CODEX_HOME/live_sessions`
//! with its request and the files it has edited, so concurrent sessions see
//! each other's work before editing and when a patch overlaps. Records expire
//! without a heartbeat. Nothing here locks or reserves a path.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

const DIRECTORY: &str = "live_sessions";
const RECORD_VERSION: u8 = 1;
/// Longer than an ordinary gap between sampling steps of a running turn.
const STALE_AFTER: Duration = Duration::from_secs(20 * 60);
const HEARTBEAT: Duration = Duration::from_secs(2 * 60);
/// How long a turn waits on `CODEX_HOME` for this register. A slower
/// operation finishes in the background, and the register stays quiet for
/// the rest of the turn instead of stalling it again.
const IO_BUDGET: Duration = Duration::from_secs(1);
const MAX_INTENT_CHARS: usize = 200;
const MAX_RECORDED_PATHS: usize = 64;
const MAX_LISTED_SESSIONS: usize = 6;
const MAX_LISTED_PATHS: usize = 12;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct LiveSessionRecord {
    version: u8,
    thread_id: String,
    workspace: String,
    intent: String,
    edited_paths: Vec<String>,
    updated_at_ms: u64,
}

/// This session's record for one turn. Drop schedules best-effort removal.
pub(crate) struct LiveSessionTurn {
    directory: PathBuf,
    path: PathBuf,
    record: LiveSessionRecord,
    written_at: Option<Instant>,
    /// Set once a file operation overran `IO_BUDGET`.
    unavailable: bool,
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
                updated_at_ms: 0,
            },
            written_at: None,
            unavailable: false,
        }
    }

    /// Publishes the files this turn has edited. Writes only when they
    /// changed or a heartbeat is due; returns whether the edited set changed.
    pub(crate) async fn publish(&mut self, edited_paths: Vec<PathBuf>) -> bool {
        if self.unavailable {
            return false;
        }
        let mut edited_paths = edited_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        edited_paths.truncate(MAX_RECORDED_PATHS);
        let changed = edited_paths != self.record.edited_paths;
        if !changed
            && self
                .written_at
                .is_some_and(|written| written.elapsed() < HEARTBEAT)
        {
            return false;
        }
        self.record.edited_paths = edited_paths;
        self.record.updated_at_ms = now_ms();
        let record = self.record.clone();
        let (directory, path) = (self.directory.clone(), self.path.clone());
        let written = bounded(tokio::task::spawn_blocking(move || {
            write_record(&directory, &path, &record).is_ok()
        }))
        .await;
        match written {
            Some(true) => self.written_at = Some(Instant::now()),
            Some(false) => {}
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
        others.unwrap_or_default()
    }

    pub(crate) fn workspace(&self) -> &Path {
        Path::new(&self.record.workspace)
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
        if workspaces_overlap(Path::new(&record.workspace), workspace) {
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

/// The context notice naming other sessions working in this checkout, or
/// nothing when there are none.
pub(crate) fn concurrent_sessions_notice(others: &[LiveSessionRecord], workspace: &Path) -> String {
    if others.is_empty() {
        return String::new();
    }
    let mut notice = String::from(
        "Other sessions are running a turn in this checkout now. Their edits are not yours: do not report them as your work, and re-read a file they edited before patching it.",
    );
    for record in others.iter().take(MAX_LISTED_SESSIONS) {
        notice.push_str("\n- Request: ");
        notice.push_str(&record.intent);
        if !record.edited_paths.is_empty() {
            notice.push_str(" Edited: ");
            notice.push_str(&listed_paths(&record.edited_paths, workspace));
        }
    }
    if others.len() > MAX_LISTED_SESSIONS {
        notice.push_str(&format!(
            "\n- {} more sessions.",
            others.len() - MAX_LISTED_SESSIONS
        ));
    }
    notice
}

/// For a patch touching `paths`, the other sessions that edited any of them.
pub(crate) fn overlapping_edits(
    others: &[LiveSessionRecord],
    paths: &[PathBuf],
    workspace: &Path,
) -> Option<String> {
    let keys = paths.iter().map(|path| path_key(path)).collect::<Vec<_>>();
    let overlaps = others
        .iter()
        .filter_map(|record| {
            let shared = record
                .edited_paths
                .iter()
                .filter(|path| keys.contains(&path_key(Path::new(path))))
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

fn bounded_intent(intent: &str) -> String {
    let line = intent
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= MAX_INTENT_CHARS {
        return line.to_string();
    }
    let mut bounded = line.chars().take(MAX_INTENT_CHARS).collect::<String>();
    bounded.push('…');
    bounded
}

fn workspaces_overlap(left: &Path, right: &Path) -> bool {
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

fn write_record(directory: &Path, path: &Path, record: &LiveSessionRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, record)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

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
        let notice = concurrent_sessions_notice(&others, second.workspace());
        assert!(notice.contains("Request: Optimize the scanner's CPU use."));
        assert!(notice.contains("Edited: lib.rs"));
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
        drop(first);
        assert_eq!(concurrent_sessions_notice(&[], &checkout), "");
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
}
