use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use crossbeam_channel::after;
use crossbeam_channel::never;
use crossbeam_channel::select;
use crossbeam_channel::unbounded;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use nucleo::Config;
use nucleo::Injector;
use nucleo::Matcher;
use nucleo::Nucleo;
use nucleo::Utf32String;
use nucleo::pattern::CaseMatching;
use nucleo::pattern::Normalization;
use serde::Serialize;
use std::fs;
use std::num::NonZero;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::RwLock;

use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

#[cfg(test)]
use nucleo::Utf32Str;
#[cfg(test)]
use nucleo::pattern::AtomKind;
#[cfg(test)]
use nucleo::pattern::Pattern;

const FILE_SEARCH_MAX_WALK_DEPTH: usize = 64;
const FILE_SEARCH_MAX_WALK_DIRECTORIES: usize = 10_000;
const FILE_SEARCH_MAX_WALK_ENTRIES: usize = 50_000;

#[derive(Clone, Copy)]
struct FileSearchWalkLimits {
    max_depth: usize,
    max_directories: usize,
    max_entries: usize,
}

const FILE_SEARCH_WALK_LIMITS: FileSearchWalkLimits = FileSearchWalkLimits {
    max_depth: FILE_SEARCH_MAX_WALK_DEPTH,
    max_directories: FILE_SEARCH_MAX_WALK_DIRECTORIES,
    max_entries: FILE_SEARCH_MAX_WALK_ENTRIES,
};

/// A single match result returned from the search.
///
/// * `score` – Relevance score returned by `nucleo`.
/// * `path`  – Path to the matched entry (file or directory), relative to the
///   search directory.
/// * `match_type` – Whether this match is a file or directory.
/// * `indices` – Optional list of character indices that matched the query.
///   These are only filled when the caller of [`run`] sets
///   `options.compute_indices` to `true`. The indices vector follows the
///   guidance from `nucleo::pattern::Pattern::indices`: they are
///   unique and sorted in ascending order so that callers can use
///   them directly for highlighting.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileMatch {
    pub score: u32,
    pub path: PathBuf,
    pub match_type: MatchType,
    pub root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indices: Option<Vec<u32>>, // Sorted & deduplicated when present
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MatchType {
    File,
    Directory,
}

impl FileMatch {
    pub fn full_path(&self) -> PathBuf {
        self.root.join(&self.path)
    }
}

/// Returns the final path component for a matched path, falling back to the full path.
pub fn file_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[derive(Debug)]
pub struct FileSearchResults {
    pub matches: Vec<FileMatch>,
    pub total_match_count: usize,
    pub scanned_file_count: usize,
    /// False if walk limits, cancellation, or traversal errors left paths unsearched.
    pub walk_complete: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct FileSearchSnapshot {
    pub query: String,
    pub matches: Vec<FileMatch>,
    pub total_match_count: usize,
    pub scanned_file_count: usize,
    /// True only after the walker finishes without omitting paths due to limits or errors.
    pub walk_complete: bool,
}

#[derive(Debug, Clone)]
pub struct FileSearchOptions {
    pub limit: NonZero<usize>,
    pub exclude: Vec<String>,
    pub threads: NonZero<usize>,
    pub compute_indices: bool,
    /// Toggle ignore-file processing in the walker.
    ///
    /// When enabled, `.gitignore` files are scoped by
    /// `WalkBuilder::require_git(true)`, so they are honored only when the
    /// traversed path is inside a git repository. When disabled, the walker
    /// turns off `.gitignore`, git-global/exclude rules, `.ignore`, and
    /// parent-directory ignore scanning.
    pub respect_gitignore: bool,
}

impl Default for FileSearchOptions {
    fn default() -> Self {
        Self {
            #[expect(clippy::unwrap_used)]
            limit: NonZero::new(20).unwrap(),
            exclude: Vec::new(),
            #[expect(clippy::unwrap_used)]
            threads: NonZero::new(2).unwrap(),
            compute_indices: false,
            respect_gitignore: true,
        }
    }
}

pub trait SessionReporter: Send + Sync + 'static {
    /// Called when the debounced top-N changes.
    fn on_update(&self, snapshot: &FileSearchSnapshot);

    /// Called with the latest query when the session becomes idle or is cancelled.
    ///
    /// Rapid query updates may be coalesced, so completion is not reported once
    /// per call to [`FileSearchSession::update_query`].
    fn on_complete(&self, query: &str);
}

pub struct FileSearchSession {
    inner: Arc<SessionInner>,
}

impl FileSearchSession {
    /// Update the query. This should be cheap relative to re-walking.
    pub fn update_query(&self, pattern_text: &str) {
        self.inner
            .latest_query
            .update(pattern_text, &self.inner.work_tx);
    }
}

impl Drop for FileSearchSession {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        let _ = self.inner.work_tx.send(WorkSignal::Shutdown);
    }
}

pub fn create_session(
    search_directories: Vec<PathBuf>,
    options: FileSearchOptions,
    reporter: Arc<dyn SessionReporter>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<FileSearchSession> {
    create_session_with_walk_limits(
        search_directories,
        options,
        reporter,
        cancel_flag,
        FILE_SEARCH_WALK_LIMITS,
    )
}

fn create_session_with_walk_limits(
    search_directories: Vec<PathBuf>,
    options: FileSearchOptions,
    reporter: Arc<dyn SessionReporter>,
    cancel_flag: Option<Arc<AtomicBool>>,
    walk_limits: FileSearchWalkLimits,
) -> anyhow::Result<FileSearchSession> {
    let FileSearchOptions {
        limit,
        exclude,
        threads,
        compute_indices,
        respect_gitignore,
    } = options;

    let Some(primary_search_directory) = search_directories.first() else {
        anyhow::bail!("at least one search directory is required");
    };
    let override_matcher = build_override_matcher(primary_search_directory, &exclude)?;
    let (work_tx, work_rx) = unbounded();
    let nucleo_notify_queued = Arc::new(AtomicBool::new(false));
    let notify = coalescing_nucleo_notify(work_tx.clone(), Arc::clone(&nucleo_notify_queued));
    let nucleo = Nucleo::new(
        Config::DEFAULT.match_paths(),
        notify,
        Some(threads.get()),
        1,
    );
    let injector = nucleo.injector();

    let cancelled = cancel_flag.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

    let inner = Arc::new(SessionInner {
        search_directories,
        limit: limit.get(),
        compute_indices,
        respect_gitignore,
        walk_limits,
        cancelled,
        shutdown: Arc::new(AtomicBool::new(false)),
        reporter,
        work_tx,
        latest_query: LatestQuery::default(),
    });

    let matcher_inner = inner.clone();
    thread::spawn(move || matcher_worker(matcher_inner, work_rx, nucleo_notify_queued, nucleo));

    let walker_inner = inner.clone();
    thread::spawn(move || walker_worker(walker_inner, override_matcher, injector));

    Ok(FileSearchSession { inner })
}

/// The worker threads will periodically check `cancel_flag` to see if they
/// should stop processing files.
pub fn run(
    pattern_text: &str,
    roots: Vec<PathBuf>,
    options: FileSearchOptions,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<FileSearchResults> {
    let reporter = Arc::new(RunReporter::default());
    let session = create_session(roots, options, reporter.clone(), cancel_flag)?;

    session.update_query(pattern_text);

    let snapshot = reporter.wait_for_complete();
    Ok(FileSearchResults {
        matches: snapshot.matches,
        total_match_count: snapshot.total_match_count,
        scanned_file_count: snapshot.scanned_file_count,
        walk_complete: snapshot.walk_complete,
    })
}

/// Sort matches in-place by descending score, then ascending path.
#[cfg(test)]
fn sort_matches(matches: &mut [(u32, String)]) {
    matches.sort_by(cmp_by_score_desc_then_path_asc::<(u32, String), _, _>(
        |t| t.0,
        |t| t.1.as_str(),
    ));
}

/// Returns a comparator closure suitable for `slice.sort_by(...)` that orders
/// items by descending score and then ascending path using the provided accessors.
pub fn cmp_by_score_desc_then_path_asc<T, FScore, FPath>(
    score_of: FScore,
    path_of: FPath,
) -> impl FnMut(&T, &T) -> std::cmp::Ordering
where
    FScore: Fn(&T) -> u32,
    FPath: Fn(&T) -> &str,
{
    use std::cmp::Ordering;
    move |a, b| match score_of(b).cmp(&score_of(a)) {
        Ordering::Equal => path_of(a).cmp(path_of(b)),
        other => other,
    }
}

#[cfg(test)]
fn create_pattern(pattern: &str) -> Pattern {
    Pattern::new(
        pattern,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    )
}

struct SessionInner {
    search_directories: Vec<PathBuf>,
    limit: usize,
    compute_indices: bool,
    respect_gitignore: bool,
    walk_limits: FileSearchWalkLimits,
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    reporter: Arc<dyn SessionReporter>,
    work_tx: Sender<WorkSignal>,
    latest_query: LatestQuery,
}

struct IndexedPath {
    full_path: Arc<str>,
    match_type: MatchType,
}

enum WorkSignal {
    QueryUpdated,
    NucleoNotify,
    WalkComplete { complete: bool },
    Shutdown,
}

#[derive(Default)]
struct LatestQuery {
    value: Mutex<String>,
    notification_queued: AtomicBool,
}

impl LatestQuery {
    fn update(&self, query: &str, work_tx: &Sender<WorkSignal>) {
        if let Ok(mut latest) = self.value.lock() {
            latest.clear();
            latest.push_str(query);
        } else {
            return;
        }
        if self
            .notification_queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if work_tx.send(WorkSignal::QueryUpdated).is_err() {
            self.notification_queued.store(false, Ordering::Release);
        }
    }

    fn read_for_worker(&self) -> String {
        self.notification_queued.store(false, Ordering::Release);
        self.value
            .lock()
            .map(|latest| latest.clone())
            .unwrap_or_default()
    }
}

fn coalescing_nucleo_notify(
    work_tx: Sender<WorkSignal>,
    notify_queued: Arc<AtomicBool>,
) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        if notify_queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if work_tx.send(WorkSignal::NucleoNotify).is_err() {
            notify_queued.store(false, Ordering::Release);
        }
    })
}

fn build_override_matcher(
    search_directory: &Path,
    exclude: &[String],
) -> anyhow::Result<Option<ignore::overrides::Override>> {
    if exclude.is_empty() {
        return Ok(None);
    }
    let mut override_builder = OverrideBuilder::new(search_directory);
    for exclude in exclude {
        let exclude_pattern = format!("!{exclude}");
        override_builder.add(&exclude_pattern)?;
    }
    let matcher = override_builder.build()?;
    Ok(Some(matcher))
}

fn get_file_path<'a>(path: &'a Path, search_directories: &[PathBuf]) -> Option<(usize, &'a str)> {
    let mut best_match: Option<(usize, &Path)> = None;
    for (idx, root) in search_directories.iter().enumerate() {
        if let Ok(rel_path) = path.strip_prefix(root) {
            let root_depth = root.components().count();
            match best_match {
                Some((best_idx, _))
                    if search_directories[best_idx].components().count() >= root_depth => {}
                _ => {
                    best_match = Some((idx, rel_path));
                }
            }
        }
    }

    let (root_idx, rel_path) = best_match?;
    rel_path.to_str().map(|p| (root_idx, p))
}

/// Walks the search directories and feeds discovered paths into `nucleo`
/// via the injector.
///
/// The walker uses `require_git(true)` to match git's own ignore semantics:
/// git never reads `.gitignore` files from directories above the repository
/// root. Without this flag, the `ignore` crate reads `.gitignore` files from
/// *all* ancestor directories—a deliberate divergence from git intended for
/// non-git use cases—allowing a broad parent ignore (e.g. `~/.gitignore`
/// containing `*`) to silently suppress every file in the walk.
///
/// When `respect_gitignore` is `false`, all git-related ignore processing is
/// disabled regardless of this flag.
fn walker_worker(
    inner: Arc<SessionInner>,
    override_matcher: Option<ignore::overrides::Override>,
    injector: Injector<IndexedPath>,
) {
    if inner.cancelled.load(Ordering::Relaxed) || inner.shutdown.load(Ordering::Relaxed) {
        let _ = inner
            .work_tx
            .send(WorkSignal::WalkComplete { complete: false });
        return;
    }
    let Some(first_root) = inner.search_directories.first() else {
        let _ = inner
            .work_tx
            .send(WorkSignal::WalkComplete { complete: true });
        return;
    };

    let mut walk_builder = WalkBuilder::new(first_root);
    for root in inner.search_directories.iter().skip(1) {
        walk_builder.add(root);
    }
    let canonical_search_directories = inner
        .search_directories
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let entries_seen = Arc::new(AtomicUsize::new(0));
    let directories_seen = Arc::new(AtomicUsize::new(0));
    let walk_limit_hit = Arc::new(AtomicBool::new(false));
    let filter_entries_seen = Arc::clone(&entries_seen);
    let filter_directories_seen = Arc::clone(&directories_seen);
    let filter_walk_limit_hit = Arc::clone(&walk_limit_hit);
    let walk_complete = Arc::new(AtomicBool::new(true));
    let filter_walk_complete = Arc::clone(&walk_complete);
    let walk_limits = inner.walk_limits;
    let filter_inner = Arc::clone(&inner);
    walk_builder
        // Allow hidden entries.
        .hidden(false)
        // Keep directory iteration streaming: sorting collects the entire
        // directory before either our work budget or cancellation can run.
        // Follow links only when their canonical targets remain in a search root.
        .follow_links(true)
        // Keep ignore behavior aligned with git repositories: only apply
        // gitignore rules when a git context exists.
        .require_git(true)
        .max_depth(Some(walk_limits.max_depth.saturating_add(1)))
        .filter_entry(move |entry| {
            if filter_inner.cancelled.load(Ordering::Relaxed)
                || filter_inner.shutdown.load(Ordering::Relaxed)
            {
                // Yield one entry so the outer loop can stop the walker.
                return true;
            }
            let is_directory = entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir());
            let max_entry_depth = walk_limits
                .max_depth
                .saturating_add(usize::from(!is_directory));
            if entry.depth() > max_entry_depth {
                filter_walk_complete.store(false, Ordering::Relaxed);
                return false;
            }
            if entry.depth() > 0
                && !reserve_walk_slot(&filter_entries_seen, walk_limits.max_entries)
            {
                filter_walk_limit_hit.store(true, Ordering::Relaxed);
                return true;
            }
            if entry.depth() > 0
                && is_directory
                && !reserve_walk_slot(&filter_directories_seen, walk_limits.max_directories)
            {
                filter_walk_limit_hit.store(true, Ordering::Relaxed);
                return true;
            }
            !entry.path_is_symlink()
                || fs::canonicalize(entry.path()).is_ok_and(|target| {
                    canonical_search_directories
                        .iter()
                        .any(|root| target.starts_with(root))
                })
        });
    walk_builder.add_custom_ignore_filename(".rgignore");
    if !inner.respect_gitignore {
        walk_builder
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    }
    if let Some(override_matcher) = override_matcher {
        walk_builder.overrides(override_matcher);
    }

    for entry in walk_builder.build() {
        if walk_limit_hit.load(Ordering::Relaxed)
            || inner.cancelled.load(Ordering::Relaxed)
            || inner.shutdown.load(Ordering::Relaxed)
        {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                walk_complete.store(false, Ordering::Relaxed);
                if !reserve_walk_slot(&entries_seen, walk_limits.max_entries) {
                    break;
                }
                continue;
            }
        };
        // The ignore walker does not call filter_entry for roots. Admit each
        // root as it is visited so excess roots retain earlier partial results.
        if entry.depth() == 0
            && entry.file_type().is_some_and(|kind| kind.is_dir())
            && !reserve_walk_slot(&directories_seen, walk_limits.max_directories)
        {
            walk_limit_hit.store(true, Ordering::Relaxed);
            break;
        }
        let path = entry.path();
        let Some(full_path) = path.to_str() else {
            continue;
        };
        if let Some((_, relative_path)) = get_file_path(path, &inner.search_directories) {
            let match_type = if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                MatchType::Directory
            } else {
                MatchType::File
            };
            injector.push(
                IndexedPath {
                    full_path: Arc::from(full_path),
                    match_type,
                },
                |_, cols| {
                    cols[0] = Utf32String::from(relative_path);
                },
            );
        }
    }
    let _ = inner.work_tx.send(WorkSignal::WalkComplete {
        complete: walk_complete.load(Ordering::Relaxed)
            && !walk_limit_hit.load(Ordering::Relaxed)
            && !inner.cancelled.load(Ordering::Relaxed)
            && !inner.shutdown.load(Ordering::Relaxed),
    });
}

fn reserve_walk_slot(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            (count < limit).then(|| count.saturating_add(1))
        })
        .is_ok()
}

fn matcher_worker(
    inner: Arc<SessionInner>,
    work_rx: Receiver<WorkSignal>,
    nucleo_notify_queued: Arc<AtomicBool>,
    mut nucleo: Nucleo<IndexedPath>,
) -> anyhow::Result<()> {
    const TICK_TIMEOUT_MS: u64 = 10;
    let config = Config::DEFAULT.match_paths();
    let mut indices_matcher = inner.compute_indices.then(|| Matcher::new(config.clone()));
    let cancel_requested = || inner.cancelled.load(Ordering::Relaxed);
    let shutdown_requested = || inner.shutdown.load(Ordering::Relaxed);

    let mut last_query: Option<String> = None;
    let mut metadata_changed = false;
    let mut next_notify = never();
    let mut will_notify = false;
    let mut walk_complete = false;
    let mut walk_finished = false;

    loop {
        if cancel_requested() || shutdown_requested() {
            break;
        }
        select! {
            recv(work_rx) -> signal => {
                let Ok(signal) = signal else {
                    break;
                };
                match signal {
                    WorkSignal::QueryUpdated => {
                        let query = inner.latest_query.read_for_worker();
                        let append = last_query.as_ref().is_some_and(|last| query.starts_with(last));
                        nucleo.pattern.reparse(
                            0,
                            &query,
                            CaseMatching::Ignore,
                            Normalization::Smart,
                            append,
                        );
                        last_query = Some(query);
                        metadata_changed = true;
                        will_notify = true;
                        next_notify = after(Duration::from_millis(0));
                    }
                    WorkSignal::NucleoNotify => {
                        nucleo_notify_queued.store(false, Ordering::Release);
                        if !will_notify {
                            will_notify = true;
                            next_notify = after(Duration::from_millis(TICK_TIMEOUT_MS));
                        }
                    }
                    WorkSignal::WalkComplete { complete } => {
                        walk_finished = true;
                        walk_complete = complete;
                        metadata_changed = true;
                        if !will_notify {
                            will_notify = true;
                            next_notify = after(Duration::from_millis(0));
                        }
                    }
                    WorkSignal::Shutdown => {
                        break;
                    }
                }
            }
            recv(next_notify) -> _ => {
                will_notify = false;
                let status = nucleo.tick(TICK_TIMEOUT_MS);
                // A running match can still expose the previous query's snapshot.
                // Compare the parsed pattern while allowing updates during the walk.
                metadata_changed |= status.changed;
                let Some(query) = last_query.as_ref() else { continue; };
                let current_pattern = &nucleo.pattern.column_pattern(0).atoms;
                let snapshot_pattern = &nucleo.snapshot().pattern().column_pattern(0).atoms;
                if metadata_changed && current_pattern == snapshot_pattern {
                    let snapshot = nucleo.snapshot();
                    let limit = inner.limit.min(snapshot.matched_item_count() as usize);
                    let pattern = snapshot.pattern().column_pattern(0);
                    let matches: Vec<_> = snapshot
                        .matches()
                        .iter()
                        .take(limit)
                        .filter_map(|match_| {
                            let item = snapshot.get_item(match_.idx)?;
                            let full_path = item.data.full_path.as_ref();
                            let (root_idx, relative_path) = get_file_path(Path::new(full_path), &inner.search_directories)?;
                            let indices = if let Some(indices_matcher) = indices_matcher.as_mut() {
                                let mut idx_vec = Vec::<u32>::new();
                                let haystack = item.matcher_columns[0].slice(..);
                                let _ = pattern.indices(haystack, indices_matcher, &mut idx_vec);
                                idx_vec.sort_unstable();
                                idx_vec.dedup();
                                Some(idx_vec)
                            } else {
                                None
                            };
                            Some(FileMatch {
                                score: match_.score,
                                path: PathBuf::from(relative_path),
                                match_type: item.data.match_type,
                                root: inner.search_directories[root_idx].clone(),
                                indices,
                            })
                        })
                        .collect();

                    let snapshot = FileSearchSnapshot {
                        query: query.clone(),
                        matches,
                        total_match_count: snapshot.matched_item_count() as usize,
                        scanned_file_count: snapshot.item_count() as usize,
                        walk_complete,
                    };
                    inner.reporter.on_update(&snapshot);
                    metadata_changed = false;
                }
                if !status.running && walk_finished {
                    inner.reporter.on_complete(query);
                }
            }
            default(Duration::from_millis(100)) => {
                // Occasionally check the cancel flag.
            }
        }

        if cancel_requested() || shutdown_requested() {
            break;
        }
    }

    // If we cancelled or otherwise exited the loop, make sure the reporter is notified.
    inner
        .reporter
        .on_complete(last_query.as_deref().unwrap_or_default());

    Ok(())
}

#[derive(Default)]
struct RunReporter {
    snapshot: RwLock<FileSearchSnapshot>,
    completed: (Condvar, Mutex<bool>),
}

impl SessionReporter for RunReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        #[allow(clippy::unwrap_used)]
        let mut guard = self.snapshot.write().unwrap();
        *guard = snapshot.clone();
    }

    fn on_complete(&self, _query: &str) {
        let (cv, mutex) = &self.completed;
        #[allow(clippy::unwrap_used)]
        let mut completed = mutex.lock().unwrap();
        *completed = true;
        cv.notify_all();
    }
}

impl RunReporter {
    fn wait_for_complete(&self) -> FileSearchSnapshot {
        let (cv, mutex) = &self.completed;
        #[allow(clippy::unwrap_used)]
        let mut completed = mutex.lock().unwrap();
        while !*completed {
            #[allow(clippy::unwrap_used)]
            {
                completed = cv.wait(completed).unwrap();
            }
        }
        #[allow(clippy::unwrap_used)]
        self.snapshot.read().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::sync::Arc;
    use std::sync::Condvar;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn verify_score_is_none_for_non_match() {
        let mut utf32buf = Vec::<char>::new();
        let line = "hello";
        let mut matcher = Matcher::new(Config::DEFAULT);
        let haystack: Utf32Str<'_> = Utf32Str::new(line, &mut utf32buf);
        let pattern = create_pattern("zzz");
        let score = pattern.score(haystack, &mut matcher);
        assert_eq!(score, None);
    }

    #[test]
    fn tie_breakers_sort_by_path_when_scores_equal() {
        let mut matches = vec![
            (100, "b_path".to_string()),
            (100, "a_path".to_string()),
            (90, "zzz".to_string()),
        ];

        sort_matches(&mut matches);

        // Highest score first; ties broken alphabetically.
        let expected = vec![
            (100, "a_path".to_string()),
            (100, "b_path".to_string()),
            (90, "zzz".to_string()),
        ];

        assert_eq!(matches, expected);
    }

    #[test]
    fn file_name_from_path_uses_basename() {
        assert_eq!(file_name_from_path("foo/bar.txt"), "bar.txt");
    }

    #[test]
    fn file_name_from_path_falls_back_to_full_path() {
        assert_eq!(file_name_from_path(""), "");
    }

    #[test]
    fn nucleo_notifications_are_coalesced_before_queueing() {
        let (work_tx, work_rx) = unbounded();
        let notify_queued = Arc::new(AtomicBool::new(false));
        let notify = coalescing_nucleo_notify(work_tx, Arc::clone(&notify_queued));

        for _ in 0..FILE_SEARCH_MAX_WALK_ENTRIES {
            notify();
        }

        assert_eq!(work_rx.len(), 1);
        assert!(matches!(work_rx.recv(), Ok(WorkSignal::NucleoNotify)));
        assert!(work_rx.is_empty());

        notify_queued.store(false, Ordering::Release);
        notify();
        assert!(matches!(work_rx.recv(), Ok(WorkSignal::NucleoNotify)));
    }

    #[test]
    fn rapid_query_updates_queue_only_the_latest_query() {
        let (work_tx, work_rx) = unbounded();
        let latest_query = LatestQuery::default();

        for index in 0..FILE_SEARCH_MAX_WALK_ENTRIES {
            latest_query.update(&format!("query-{index}"), &work_tx);
        }

        assert_eq!(work_rx.len(), 1);
        assert!(matches!(work_rx.recv(), Ok(WorkSignal::QueryUpdated)));
        assert_eq!(
            latest_query.read_for_worker(),
            format!("query-{}", FILE_SEARCH_MAX_WALK_ENTRIES - 1)
        );
        assert!(work_rx.is_empty());

        latest_query.update("next-query", &work_tx);
        assert!(matches!(work_rx.recv(), Ok(WorkSignal::QueryUpdated)));
        assert_eq!(latest_query.read_for_worker(), "next-query");
    }

    #[derive(Default)]
    struct RecordingReporter {
        updates: Mutex<Vec<FileSearchSnapshot>>,
        complete_times: Mutex<Vec<Instant>>,
        complete_cv: Condvar,
        update_cv: Condvar,
    }

    impl RecordingReporter {
        fn wait_until<T, F>(
            &self,
            mutex: &Mutex<T>,
            cv: &Condvar,
            timeout: Duration,
            mut predicate: F,
        ) -> bool
        where
            F: FnMut(&T) -> bool,
        {
            let deadline = Instant::now() + timeout;
            let mut state = mutex.lock().unwrap();
            loop {
                if predicate(&state) {
                    return true;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                let (next_state, wait_result) = cv.wait_timeout(state, remaining).unwrap();
                state = next_state;
                if wait_result.timed_out() {
                    return predicate(&state);
                }
            }
        }

        fn wait_for_complete(&self, timeout: Duration) -> bool {
            self.wait_until(
                &self.complete_times,
                &self.complete_cv,
                timeout,
                |completes| !completes.is_empty(),
            )
        }
        fn clear(&self) {
            self.updates.lock().unwrap().clear();
            self.complete_times.lock().unwrap().clear();
        }

        fn updates(&self) -> Vec<FileSearchSnapshot> {
            self.updates.lock().unwrap().clone()
        }

        fn wait_for_updates_at_least(&self, min_len: usize, timeout: Duration) -> bool {
            self.wait_until(&self.updates, &self.update_cv, timeout, |updates| {
                updates.len() >= min_len
            })
        }

        fn snapshot(&self) -> FileSearchSnapshot {
            self.updates
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    impl SessionReporter for RecordingReporter {
        fn on_update(&self, snapshot: &FileSearchSnapshot) {
            let mut updates = self.updates.lock().unwrap();
            updates.push(snapshot.clone());
            self.update_cv.notify_all();
        }

        fn on_complete(&self, _query: &str) {
            {
                let mut complete_times = self.complete_times.lock().unwrap();
                complete_times.push(Instant::now());
            }
            self.complete_cv.notify_all();
        }
    }

    fn create_temp_tree(file_count: usize) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..file_count {
            let path = dir.path().join(format!("file-{i:04}.txt"));
            fs::write(path, format!("contents {i}")).unwrap();
        }
        dir
    }

    fn run_with_walk_limits(
        pattern_text: &str,
        roots: Vec<PathBuf>,
        walk_limits: FileSearchWalkLimits,
    ) -> FileSearchSnapshot {
        let reporter = Arc::new(RunReporter::default());
        let session = create_session_with_walk_limits(
            roots,
            FileSearchOptions {
                threads: NonZero::new(1).unwrap(),
                ..Default::default()
            },
            reporter.clone(),
            /*cancel_flag*/ None,
            walk_limits,
        )
        .expect("session");
        session.update_query(pattern_text);
        let snapshot = reporter.wait_for_complete();
        drop(session);
        snapshot
    }

    #[cfg(windows)]
    fn try_symlink_directory(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    #[cfg(unix)]
    fn try_symlink_directory(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).expect("create directory symlink");
        true
    }

    #[test]
    fn fuzzy_walk_respects_explicit_depth_directory_and_entry_bounds() {
        let depth_root = tempfile::tempdir().unwrap();
        fs::write(depth_root.path().join("root-needle.txt"), "root").unwrap();
        fs::create_dir(depth_root.path().join("nested")).unwrap();
        fs::write(depth_root.path().join("nested/deep-needle.txt"), "deep").unwrap();
        let depth_snapshot = run_with_walk_limits(
            "needle",
            vec![depth_root.path().to_path_buf()],
            FileSearchWalkLimits {
                max_depth: 0,
                max_directories: FILE_SEARCH_MAX_WALK_DIRECTORIES,
                max_entries: FILE_SEARCH_MAX_WALK_ENTRIES,
            },
        );
        assert!(
            depth_snapshot
                .matches
                .iter()
                .all(|file_match| !file_match.path.starts_with("nested"))
        );
        assert!(
            depth_snapshot
                .matches
                .iter()
                .any(|m| m.path == Path::new("root-needle.txt"))
        );
        assert!(!depth_snapshot.walk_complete);

        let directory_snapshot = run_with_walk_limits(
            "needle",
            vec![depth_root.path().to_path_buf()],
            FileSearchWalkLimits {
                max_depth: FILE_SEARCH_MAX_WALK_DEPTH,
                max_directories: 1,
                max_entries: FILE_SEARCH_MAX_WALK_ENTRIES,
            },
        );
        assert!(
            directory_snapshot
                .matches
                .iter()
                .all(|file_match| !file_match.path.starts_with("nested"))
        );
        assert!(!directory_snapshot.walk_complete);
        let allowed_root = tempfile::tempdir().unwrap();
        fs::write(allowed_root.path().join("allowed-needle.txt"), "allowed").unwrap();
        let allowed_snapshot = run_with_walk_limits(
            "needle",
            vec![allowed_root.path().to_path_buf()],
            FileSearchWalkLimits {
                max_depth: 0,
                max_directories: 1,
                max_entries: 1,
            },
        );
        assert_eq!(allowed_snapshot.matches.len(), 1);
        assert!(allowed_snapshot.walk_complete);
        assert_eq!(
            allowed_snapshot.matches[0].path,
            Path::new("allowed-needle.txt")
        );

        let entry_root = tempfile::tempdir().unwrap();
        fs::write(entry_root.path().join("b-needle.txt"), "b").unwrap();
        fs::write(entry_root.path().join("a-needle.txt"), "a").unwrap();
        let entry_snapshot = run_with_walk_limits(
            "needle",
            vec![entry_root.path().to_path_buf()],
            FileSearchWalkLimits {
                max_depth: FILE_SEARCH_MAX_WALK_DEPTH,
                max_directories: FILE_SEARCH_MAX_WALK_DIRECTORIES,
                max_entries: 1,
            },
        );
        assert_eq!(entry_snapshot.matches.len(), 1);
        assert!(
            [Path::new("a-needle.txt"), Path::new("b-needle.txt")]
                .contains(&entry_snapshot.matches[0].path.as_path())
        );
        assert!(!entry_snapshot.walk_complete);
    }

    #[test]
    fn fuzzy_walk_large_flat_directory_stops_at_entry_budget() {
        let root = create_temp_tree(10_000);
        fs::write(root.path().join("File-99999.txt"), "sort sentinel").unwrap();
        let native_entries = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| PathBuf::from(entry.unwrap().file_name()))
            .collect::<Vec<_>>();
        let mut expected = native_entries.iter().take(8).cloned().collect::<Vec<_>>();
        expected.sort();
        let mut sorted_entries = native_entries;
        sorted_entries.sort();
        assert_ne!(
            expected,
            sorted_entries[..8],
            "the fixture must distinguish native traversal from eager filename sorting"
        );
        let snapshot = run_with_walk_limits(
            "file-",
            vec![root.path().to_path_buf()],
            FileSearchWalkLimits {
                max_depth: 0,
                max_directories: 1,
                max_entries: 8,
            },
        );
        assert_eq!(snapshot.total_match_count, 8);
        assert_eq!(snapshot.matches.len(), 8);
        assert!(!snapshot.walk_complete);
        let mut actual = snapshot
            .matches
            .into_iter()
            .map(|found| found.path)
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(
            actual, expected,
            "the budget ends native streaming traversal"
        );
    }

    #[test]
    fn fuzzy_walk_preserves_results_before_exceeding_root_directory_budget() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fs::write(first.path().join("first-needle.txt"), "first").unwrap();
        fs::write(second.path().join("second-needle.txt"), "second").unwrap();

        for max_directories in 0..=2 {
            let snapshot = run_with_walk_limits(
                "needle",
                vec![first.path().to_path_buf(), second.path().to_path_buf()],
                FileSearchWalkLimits {
                    max_directories,
                    ..FILE_SEARCH_WALK_LIMITS
                },
            );
            let mut paths = snapshot
                .matches
                .iter()
                .map(|file_match| file_match.path.clone())
                .collect::<Vec<_>>();
            paths.sort();
            let expected = ["first-needle.txt", "second-needle.txt"]
                .into_iter()
                .take(max_directories)
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            assert_eq!(paths, expected, "directory budget {max_directories}");
            assert_eq!(snapshot.walk_complete, max_directories == 2);
        }
    }

    #[test]
    fn fuzzy_walk_follows_only_symlinks_confined_to_search_roots() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let inside = workspace.join("inside");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(inside.join("inside-target.txt"), "inside").unwrap();
        fs::write(outside.join("outside-target.txt"), "outside").unwrap();
        if !try_symlink_directory(&inside, &workspace.join("inside-link"))
            || !try_symlink_directory(&outside, &workspace.join("outside-link"))
        {
            return;
        }

        let inside_results = run(
            "inside-target",
            vec![workspace.clone()],
            FileSearchOptions::default(),
            /*cancel_flag*/ None,
        )
        .expect("inside search");
        assert!(inside_results.matches.iter().any(|file_match| {
            file_match.path == Path::new("inside-link").join("inside-target.txt")
        }));

        let outside_results = run(
            "outside-target",
            vec![workspace],
            FileSearchOptions::default(),
            /*cancel_flag*/ None,
        )
        .expect("outside search");
        assert!(outside_results.matches.is_empty());
    }

    #[test]
    fn session_scanned_file_count_is_monotonic_across_queries() {
        let dir = create_temp_tree(/*file_count*/ 200);
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("file-00");
        thread::sleep(Duration::from_millis(20));
        let first_snapshot = reporter.snapshot();
        session.update_query("file-01");
        thread::sleep(Duration::from_millis(20));
        let second_snapshot = reporter.snapshot();
        let _ = reporter.wait_for_complete(Duration::from_secs(5));
        let completed_snapshot = reporter.snapshot();

        assert!(second_snapshot.scanned_file_count >= first_snapshot.scanned_file_count);
        assert!(completed_snapshot.scanned_file_count >= second_snapshot.scanned_file_count);
    }

    #[test]
    fn session_streams_updates_before_walk_complete() {
        let dir = create_temp_tree(/*file_count*/ 1);
        let reporter = Arc::new(RecordingReporter::default());
        // Hold WalkComplete until a real injected match has been published.
        let (work_tx, work_rx) = unbounded();
        let queued = Arc::new(AtomicBool::new(false));
        let nucleo = Nucleo::new(
            Config::DEFAULT.match_paths(),
            coalescing_nucleo_notify(work_tx.clone(), queued.clone()),
            Some(1),
            1,
        );
        let injector = nucleo.injector();
        let inner = Arc::new(SessionInner {
            search_directories: vec![dir.path().to_path_buf()],
            limit: 20,
            compute_indices: false,
            respect_gitignore: true,
            walk_limits: FILE_SEARCH_WALK_LIMITS,
            cancelled: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            reporter: reporter.clone(),
            work_tx: work_tx.clone(),
            latest_query: LatestQuery::default(),
        });
        let matcher_inner = inner.clone();
        let worker = thread::spawn(move || matcher_worker(matcher_inner, work_rx, queued, nucleo));
        let session = FileSearchSession { inner };
        let path = dir.path().join("file-0000.txt");
        injector.push(
            IndexedPath {
                full_path: Arc::from(path.to_str().unwrap()),
                match_type: MatchType::File,
            },
            |_, cols| {
                cols[0] = Utf32String::from("file-0000.txt");
            },
        );
        session.update_query("file-0");
        assert!(reporter.wait_for_updates_at_least(1, Duration::from_secs(5)));
        let snapshot = reporter.snapshot();
        assert!(!snapshot.walk_complete);
        assert_eq!(snapshot.query, "file-0");
        assert_eq!(snapshot.matches.len(), 1);
        assert_eq!(snapshot.matches[0].path, Path::new("file-0000.txt"));
        work_tx
            .send(WorkSignal::WalkComplete { complete: true })
            .unwrap();
        let completed = reporter.wait_for_complete(Duration::from_secs(5));
        assert!(completed);
        assert!(reporter.snapshot().walk_complete);
        assert_eq!(reporter.snapshot().matches, snapshot.matches);
        drop(session);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn session_waits_for_submitted_query_including_empty_query() {
        let dir = create_temp_tree(1);
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            None,
        )
        .unwrap();
        assert!(!reporter.wait_for_complete(Duration::from_millis(100)));
        assert!(reporter.updates().is_empty());
        session.update_query("");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        let snapshot = reporter.snapshot();
        assert!(snapshot.walk_complete);
        assert!(
            snapshot
                .matches
                .iter()
                .any(|m| m.path == Path::new("file-0000.txt"))
        );
    }

    #[test]
    fn session_accepts_query_updates_after_walk_complete() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.txt"), "alpha").unwrap();
        fs::write(dir.path().join("beta.txt"), "beta").unwrap();
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("alpha");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        let updates_before = reporter.updates().len();

        session.update_query("beta");
        assert!(reporter.wait_for_updates_at_least(updates_before + 1, Duration::from_secs(5),));

        let updates = reporter.updates();
        let last_update = updates.last().cloned().expect("update");
        assert!(
            last_update
                .matches
                .iter()
                .any(|file_match| file_match.path.to_string_lossy().contains("beta.txt"))
        );
    }

    #[test]
    fn session_emits_complete_when_query_changes_with_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.txt"), "alpha").unwrap();
        fs::write(dir.path().join("beta.txt"), "beta").unwrap();
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("asdf");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));

        let completed_snapshot = reporter.snapshot();
        assert_eq!(completed_snapshot.matches, Vec::new());
        assert_eq!(completed_snapshot.total_match_count, 0);

        reporter.clear();

        session.update_query("asdfa");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        assert!(!reporter.updates().is_empty());
    }

    #[test]
    fn dropping_session_does_not_cancel_siblings_with_shared_cancel_flag() {
        let root_a = create_temp_tree(/*file_count*/ 200);
        let root_b = create_temp_tree(/*file_count*/ 4_000);
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let reporter_a = Arc::new(RecordingReporter::default());
        let session_a = create_session(
            vec![root_a.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter_a,
            Some(cancel_flag.clone()),
        )
        .expect("session_a");

        let reporter_b = Arc::new(RecordingReporter::default());
        let session_b = create_session(
            vec![root_b.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter_b.clone(),
            Some(cancel_flag),
        )
        .expect("session_b");

        session_a.update_query("file-0");
        session_b.update_query("file-1");

        thread::sleep(Duration::from_millis(5));
        drop(session_a);

        let completed = reporter_b.wait_for_complete(Duration::from_secs(5));
        assert_eq!(completed, true);
    }

    #[test]
    fn session_emits_updates_when_query_changes() {
        let dir = create_temp_tree(/*file_count*/ 200);
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("zzzzzzzz");
        let completed = reporter.wait_for_complete(Duration::from_secs(5));
        assert!(completed);

        reporter.clear();

        session.update_query("zzzzzzzzq");
        let completed = reporter.wait_for_complete(Duration::from_secs(5));
        assert!(completed);

        let updates = reporter.updates();
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn run_returns_matches_for_query() {
        let dir = create_temp_tree(/*file_count*/ 40);
        let options = FileSearchOptions {
            limit: NonZero::new(20).unwrap(),
            exclude: Vec::new(),
            threads: NonZero::new(2).unwrap(),
            compute_indices: false,
            respect_gitignore: true,
        };
        let results = run(
            "file-",
            vec![dir.path().to_path_buf()],
            options,
            /*cancel_flag*/ None,
        )
        .expect("run ok");

        assert_eq!(results.matches.len(), 20);
        assert_eq!(results.total_match_count, 40);
        // The index includes the root directory as well as its 40 files.
        assert_eq!(results.scanned_file_count, 41);
        assert!(results.walk_complete);
        assert!(
            results
                .matches
                .iter()
                .all(|m| m.match_type == MatchType::File && dir.path().join(&m.path).is_file())
        );
    }

    #[test]
    fn disabling_gitignore_preserves_local_and_parent_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(dir.path().join(".ignore"), "needle-parent.txt\n").unwrap();
        fs::write(repo.join(".ignore"), "needle-local.txt\n").unwrap();
        fs::write(repo.join(".rgignore"), "needle-rg.txt\n").unwrap();
        fs::write(repo.join(".gitignore"), "needle-git.txt\n").unwrap();
        for name in [
            "needle-parent.txt",
            "needle-local.txt",
            "needle-rg.txt",
            "needle-git.txt",
            "needle-visible.txt",
        ] {
            fs::write(repo.join(name), "contents").unwrap();
        }
        for respect_gitignore in [false, true] {
            let results = run(
                "needle",
                vec![repo.clone()],
                FileSearchOptions {
                    respect_gitignore,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            let mut paths = results
                .matches
                .into_iter()
                .map(|entry| entry.path)
                .collect::<Vec<_>>();
            paths.sort();
            let expected = if respect_gitignore {
                vec![PathBuf::from("needle-visible.txt")]
            } else {
                vec![
                    PathBuf::from("needle-git.txt"),
                    PathBuf::from("needle-visible.txt"),
                ]
            };
            assert_eq!(paths, expected);
            assert!(results.walk_complete);
        }
    }

    #[test]
    fn session_reuses_indexed_file_types_after_query_updates() {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().join("needle-directory");
        let file = dir.path().join("needle-file.txt");
        fs::create_dir(&directory).unwrap();
        fs::write(&file, "contents").unwrap();
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            None,
        )
        .unwrap();
        session.update_query("needle");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        let expected = [
            (PathBuf::from("needle-directory"), MatchType::Directory),
            (PathBuf::from("needle-file.txt"), MatchType::File),
        ];
        let indexed_types = |snapshot: FileSearchSnapshot| {
            let mut types: Vec<_> = snapshot
                .matches
                .into_iter()
                .map(|entry| (entry.path, entry.match_type))
                .collect();
            types.sort_by(|a, b| a.0.cmp(&b.0));
            types
        };
        assert_eq!(indexed_types(reporter.snapshot()), expected);

        // Query changes reuse the same walk snapshot, including its metadata.
        // Swapping the live types exposes accidental per-result filesystem reads.
        fs::remove_dir(&directory).unwrap();
        fs::write(&directory, "now a file").unwrap();
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        reporter.clear();
        session.update_query("needle-");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        assert_eq!(reporter.snapshot().query, "needle-");
        assert_eq!(indexed_types(reporter.snapshot()), expected);
    }

    #[test]
    fn run_returns_directory_matches_for_query() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("docs/guides")).unwrap();
        fs::write(dir.path().join("docs/guides/intro.md"), "intro").unwrap();
        fs::write(dir.path().join("docs/readme.md"), "readme").unwrap();

        let results = run(
            "guides",
            vec![dir.path().to_path_buf()],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");

        assert!(results.matches.iter().any(|m| {
            m.path == std::path::Path::new("docs").join("guides")
                && m.match_type == MatchType::Directory
        }));
    }

    #[test]
    fn cancel_exits_run() {
        let dir = create_temp_tree(/*file_count*/ 200);
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let search_dir = dir.path().to_path_buf();
        let options = FileSearchOptions {
            compute_indices: false,
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();

        let handle = thread::spawn(move || {
            let result = run("file-", vec![search_dir], options, Some(cancel_flag));
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("run should exit after cancellation");
        handle.join().unwrap();

        let results = result.expect("run ok");
        assert_eq!(results.matches, Vec::new());
        assert_eq!(results.total_match_count, 0);
    }

    /// Regression test for #3493: a parent directory's `.gitignore` with `*`
    /// must not suppress files discovered inside a child "repo" directory.
    ///
    /// The fixture intentionally omits `git init` so that no `.git` directory
    /// exists. With `require_git(true)`, the walker skips all gitignore
    /// processing, making the parent's broad ignore harmless.
    #[test]
    fn parent_gitignore_outside_repo_does_not_hide_repo_files() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("home");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".vscode")).unwrap();

        fs::write(parent.join(".gitignore"), "*\n!.gitignore\n").unwrap();
        fs::write(
            repo.join(".gitignore"),
            ".vscode/*\n!.vscode/\n!.vscode/settings.json\n!package.json\n",
        )
        .unwrap();
        fs::write(repo.join("package.json"), "{ \"name\": \"demo\" }\n").unwrap();
        fs::write(repo.join(".vscode/settings.json"), "{ \"editor\": true }\n").unwrap();

        let respect_results = run(
            "package",
            vec![repo.clone()],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            respect_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new("package.json"))
        );

        let nested_file_results = run(
            "settings",
            vec![repo],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            nested_file_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new(".vscode/settings.json"))
        );
    }

    #[test]
    fn git_repo_still_respects_local_gitignore_when_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("home");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".vscode")).unwrap();

        fs::write(parent.join(".gitignore"), "*\n!.gitignore\n").unwrap();
        fs::write(
            repo.join(".gitignore"),
            ".vscode/*\n!.vscode/\n!.vscode/settings.json\n!package.json\n",
        )
        .unwrap();
        fs::write(repo.join("package.json"), "{ \"name\": \"demo\" }\n").unwrap();
        fs::write(repo.join(".vscode/settings.json"), "{ \"editor\": true }\n").unwrap();
        fs::write(
            repo.join(".vscode/extensions.json"),
            "{ \"extensions\": [] }\n",
        )
        .unwrap();

        fs::create_dir_all(repo.join(".git")).unwrap();

        let package_results = run(
            "package",
            vec![repo.clone()],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            package_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new("package.json"))
        );

        let ignored_results = run(
            "extensions.json",
            vec![repo.clone()],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            !ignored_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new(".vscode/extensions.json"))
        );

        let whitelisted_results = run(
            "settings.json",
            vec![repo],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            whitelisted_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new(".vscode/settings.json"))
        );
    }
}
