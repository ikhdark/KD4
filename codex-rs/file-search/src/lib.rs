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
use nucleo::pattern::Atom;
use nucleo::pattern::AtomKind;
use nucleo::pattern::CaseMatching;
use nucleo::pattern::Normalization;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;
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
use unicode_segmentation::UnicodeSegmentation;


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
/// * `indices` – Optional list of character indices into `path` that matched
///   the query. These are only filled when the caller of [`run`] sets
///   `options.compute_indices` to `true`. They are unique and sorted in
///   ascending order so that callers can use them directly for highlighting.
///   A matched multi-character grapheme contributes all of its characters.
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

#[derive(Debug)]
pub struct FileSearchResults {
    pub matches: Vec<FileMatch>,
    pub total_match_count: usize,
    pub scanned_file_count: usize,
    /// False if walk limits, cancellation, traversal errors, or ignore-file errors
    /// prevented a reliable complete search.
    pub walk_complete: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct FileSearchSnapshot {
    pub query: String,
    pub matches: Vec<FileMatch>,
    pub total_match_count: usize,
    pub scanned_file_count: usize,
    /// True only after the walker finishes without limits, traversal errors, or ignore-file errors.
    pub walk_complete: bool,
    /// The current query has evaluated the entire admitted walk snapshot.
    pub match_complete: bool,
    /// Cancellation interrupted this query, rather than stopping an already idle query.
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
pub struct FileSearchOptions {
    pub limit: NonZero<usize>,
    pub exclude: Vec<String>,
    pub threads: NonZero<usize>,
    pub compute_indices: bool,
    /// Toggle git ignore-file processing in the walker.
    ///
    /// When enabled, `.gitignore` files are scoped by
    /// `WalkBuilder::require_git(true)`, so they are honored only when the
    /// traversed path is inside a git repository. When disabled, the walker
    /// turns off `.gitignore` and git-global/exclude rules. Local and ancestor
    /// `.ignore` and `.rgignore` rules remain active in either mode.
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

    /// Terminal data is published before completion, including interrupted searches.
    fn on_complete_snapshot(&self, snapshot: &FileSearchSnapshot) {
        self.on_complete(&snapshot.query);
    }
}

pub struct FileSearchSession {
    inner: Arc<SessionInner>,
}

impl FileSearchSession {
    /// Update the query. This should be cheap relative to re-walking.
    /// Queries reuse the session's walk snapshot; create a new session to discover
    /// subsequent filesystem changes.
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

    if search_directories.is_empty() {
        anyhow::bail!("at least one search directory is required");
    }
    // Validate every root's relative override context before starting workers.
    let mut walk_roots = Vec::new();
    for root in &search_directories {
        if !walk_roots.iter().any(|(path, _)| path == root) {
            walk_roots.push((root.clone(), build_override_matcher(root, &exclude)?));
        }
    }
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
    thread::spawn(move || walker_worker(walker_inner, walk_roots, injector));

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
        walk_complete: snapshot.walk_complete && snapshot.match_complete && !snapshot.cancelled,
    })
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
    relative_start: usize,
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
    value: Mutex<QuerySubmission>,
    notification_queued: AtomicBool,
}

#[derive(Clone, Default)]
struct QuerySubmission {
    text: String,
    generation: usize,
}

impl LatestQuery {
    fn update(&self, query: &str, work_tx: &Sender<WorkSignal>) {
        if let Ok(mut latest) = self.value.lock() {
            latest.text.clear();
            latest.text.push_str(query);
            latest.generation = latest.generation.wrapping_add(1);
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

    fn read_for_worker(&self) -> QuerySubmission {
        self.notification_queued.store(false, Ordering::Release);
        self.current()
    }

    fn current(&self) -> QuerySubmission {
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

fn walker_worker(
    inner: Arc<SessionInner>,
    walk_roots: Vec<(PathBuf, Option<ignore::overrides::Override>)>,
    injector: Injector<IndexedPath>,
) {
    let stopped =
        || inner.cancelled.load(Ordering::Relaxed) || inner.shutdown.load(Ordering::Relaxed);
    let canonical_roots = inner
        .search_directories
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let entries_seen = AtomicUsize::new(0);
    let directories_seen = AtomicUsize::new(0);
    let limits = inner.walk_limits;
    let mut complete = true;

    'roots: for (root, overrides) in walk_roots {
        if stopped() {
            complete = false;
            break;
        }
        let mut builder = WalkBuilder::new(&root);
        builder
            .hidden(false)
            .follow_links(true)
            .require_git(true)
            .add_custom_ignore_filename(".rgignore");
        if !inner.respect_gitignore {
            builder
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false);
        }
        if let Some(overrides) = overrides {
            builder.overrides(overrides);
        }
        // Reuse ignore's hierarchy/override parser, but own the raw advance:
        // ignore::Walk::next may otherwise consume an entire ignored directory
        // without returning to our cancellation observation.
        let mut ignore = builder.build_matchers().remove(0);
        // The incremental matcher skips an empty relative path. Initialize
        // its root/ancestor ignore state even when the root has no children.
        // This synthetic name is matched only; no source path is opened.
        if ignore.matched_with_errors(".codex-ignore-initialization", true).1.is_some() {
            complete = false;
        }
        let mut walk = walkdir::WalkDir::new(&root)
            .follow_links(true)
            .max_depth(limits.max_depth.saturating_add(1))
            .into_iter();
        loop {
            if stopped() {
                complete = false;
                break 'roots;
            }
            let Some(entry) = walk.next() else {
                break;
            };
            if stopped() {
                complete = false;
                break 'roots;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    complete = false;
                    if !reserve_walk_slot(&entries_seen, limits.max_entries) {
                        break 'roots;
                    }
                    continue;
                }
            };
            let is_dir = entry.file_type().is_dir();
            let depth = entry.depth();
            if depth > 0
                && (is_vcs_metadata(entry.file_name())
                    || (is_dir
                        && inner
                            .search_directories
                            .iter()
                            .any(|nested| nested != &root && nested == entry.path())))
            {
                if is_dir {
                    walk.skip_current_dir();
                }
                continue;
            }
            if entry.path_is_symlink()
                && !fs::canonicalize(entry.path())
                    .is_ok_and(|target| canonical_roots.iter().any(|root| target.starts_with(root)))
            {
                if is_dir {
                    walk.skip_current_dir();
                }
                continue;
            }
            let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            let (matched, error) = ignore.matched_with_errors(relative, is_dir);
            if error.is_some() {
                complete = false;
            }
            if matched.is_ignore() {
                if is_dir {
                    walk.skip_current_dir();
                }
                continue;
            }
            if depth > limits.max_depth.saturating_add(usize::from(!is_dir)) {
                complete = false;
                if is_dir {
                    walk.skip_current_dir();
                }
                continue;
            }
            if depth > 0 && !reserve_walk_slot(&entries_seen, limits.max_entries) {
                complete = false;
                break 'roots;
            }
            if is_dir && !reserve_walk_slot(&directories_seen, limits.max_directories) {
                complete = false;
                break 'roots;
            }
            let Some(full_path) = entry.path().to_str() else {
                complete = false;
                continue;
            };
            if let Some((_, relative_path)) = get_file_path(entry.path(), &inner.search_directories)
            {
                injector.push(
                    IndexedPath {
                        full_path: Arc::from(full_path),
                        relative_start: full_path.len() - relative_path.len(),
                        match_type: if is_dir {
                            MatchType::Directory
                        } else {
                            MatchType::File
                        },
                    },
                    |_, cols| cols[0] = Utf32String::from(&*match_text(relative_path)),
                );
            }
        }
    }
    let _ = inner.work_tx.send(WorkSignal::WalkComplete {
        complete: complete && !stopped(),
    });
}

/// Maps sorted nucleo match positions to the character positions documented
/// on [`FileMatch::indices`].
///
/// nucleo indexes non-ASCII haystacks by extended grapheme cluster, so each
/// matched cluster expands to every character it contains.
fn grapheme_indices_to_char_indices(haystack: &str, grapheme_indices: &[u32]) -> Vec<u32> {
    let mut char_indices = Vec::with_capacity(grapheme_indices.len());
    let mut remaining = grapheme_indices.iter().copied().peekable();
    let mut char_start = 0u32;
    for (grapheme_index, grapheme) in (0u32..).zip(haystack.graphemes(/*is_extended*/ true)) {
        let Some(&next) = remaining.peek() else {
            break;
        };
        let char_count = grapheme.chars().count() as u32;
        if next == grapheme_index {
            remaining.next();
            char_indices.extend(char_start..char_start + char_count);
        }
        char_start += char_count;
    }
    char_indices
}

/// Text the matcher indexes for a relative path. Windows paths are indexed
/// with `/` separators so that queries typed with either separator match; the
/// one-for-one replacement keeps match indices valid for the native path.
fn match_text(relative_path: &str) -> Cow<'_, str> {
    if cfg!(windows) && relative_path.contains('\\') {
        Cow::Owned(relative_path.replace('\\', "/"))
    } else {
        Cow::Borrowed(relative_path)
    }
}

/// Rewrites Windows path separators in a query to the `/` that [`match_text`]
/// indexes. A backslash before a character the pattern syntax can escape
/// (space, `!`, `^`, `'`, `$`) keeps its escape meaning.
fn match_query(query: &str) -> Cow<'_, str> {
    if !cfg!(windows) || !query.contains('\\') {
        return Cow::Borrowed(query);
    }
    let mut normalized = String::with_capacity(query.len());
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        let escapes = matches!(chars.peek(), Some(' ' | '!' | '^' | '\'' | '$'));
        normalized.push(if c == '\\' && !escapes { '/' } else { c });
    }
    Cow::Owned(normalized)
}

/// Version-control metadata is never a useful result, and a `.git` directory
/// alone can hold more entries than the walk budget, starving the worktree.
const VCS_METADATA_NAMES: [&str; 4] = [".git", ".hg", ".jj", ".svn"];

fn is_vcs_metadata(name: &OsStr) -> bool {
    VCS_METADATA_NAMES
        .iter()
        .any(|metadata_name| name.eq_ignore_ascii_case(metadata_name))
}

fn reserve_walk_slot(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            (count < limit).then(|| count.saturating_add(1))
        })
        .is_ok()
}

/// Plain ASCII path growth cannot reinterpret operators or change normalization.
/// Other edits rematch the retained index, never the filesystem.
fn can_filter_previous_candidates(previous: &str, next: &str) -> bool {
    !previous.is_empty()
        && next.starts_with(previous)
        && next
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_./-".contains(&byte))
}

fn literal_query(query: &str) -> Option<String> {
    let pattern =
        nucleo::pattern::Pattern::parse(query, CaseMatching::Respect, Normalization::Never);
    let [atom] = pattern.atoms.as_slice() else {
        return None;
    };
    if atom.negative || atom.kind != AtomKind::Fuzzy {
        return None;
    }
    // Nucleo's grapheme approximation is matching text, not native spelling.
    // Keep unescaped Unicode verbatim for the byte-exact relevance tiers.
    if !query.contains('\\') && !query.chars().any(char::is_whitespace) {
        Some(query.to_owned())
    } else if query.is_ascii() {
        Some(atom.needle_text().chars().collect())
    } else {
        None
    }
}

fn literal_rank(
    item: &nucleo::Item<'_, IndexedPath>,
    literal: Option<&str>,
    exact: Option<&Atom>,
    matcher: &mut Matcher,
) -> u8 {
    let Some(literal) = literal else {
        return 3;
    };
    let relative = &item.data.full_path[item.data.relative_start..];
    let same_path = relative
        .chars()
        .map(|c| if cfg!(windows) && c == '\\' { '/' } else { c })
        .eq(literal.chars());
    if same_path {
        return 0;
    }
    let basename = Path::new(relative)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(relative);
    if basename == literal {
        return 1;
    }
    let column = item.matcher_columns[0].slice(..);
    let start = column
        .chars()
        .enumerate()
        .filter_map(|(index, c)| (c == '/').then_some(index + 1))
        .last()
        .unwrap_or(0);
    if exact.is_some_and(|atom| atom.score(column.slice(start..), matcher).is_some()) {
        2
    } else {
        3
    }
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
    let mut rank_matcher = Matcher::new(config);
    let cancel_requested = || inner.cancelled.load(Ordering::Relaxed);
    let shutdown_requested = || inner.shutdown.load(Ordering::Relaxed);

    let mut last_query: Option<QuerySubmission> = None;
    let mut literal = None;
    let mut exact = None;
    // This session only appends immutable indexed paths. Scores change as
    // matching advances, but a path's relevance tier changes only with query.
    let mut literal_tiers = HashMap::new();
    let mut last_snapshot = FileSearchSnapshot::default();
    let mut completed_generation = None;
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
                        let pattern_text = match_query(&query.text);
                        let append = last_query
                            .as_ref()
                            .is_some_and(|last| can_filter_previous_candidates(&match_query(&last.text), &pattern_text));
                        nucleo.pattern.reparse(
                            0,
                            &pattern_text,
                            CaseMatching::Ignore,
                            Normalization::Smart,
                            append,
                        );
                        literal = literal_query(&pattern_text);
                        exact = literal.as_deref().map(|literal| Atom::new(literal, CaseMatching::Ignore, Normalization::Smart, AtomKind::Exact, false));
                        literal_tiers.clear();
                        last_query = Some(query);
                        last_snapshot = FileSearchSnapshot::default();
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
                    // Select within the retained matcher snapshot, not the
                    // filesystem walk. Nucleo's insertion-index tie breaker
                    // must not determine membership at the cutoff.
                    let mut ranked = snapshot
                        .matches()
                        .iter()
                        .filter_map(|matched| {
                            let item = snapshot.get_item(matched.idx)?;
                            let tier = if literal.is_some() {
                                *literal_tiers.entry(matched.idx).or_insert_with(|| {
                                    literal_rank(&item, literal.as_deref(), exact.as_ref(), &mut rank_matcher)
                                })
                            } else {
                                3
                            };
                            Some((matched, item.matcher_columns[0].slice(..).len(), item.data.full_path.as_ref(), tier))
                        })
                        .collect::<Vec<_>>();
                    let rank = |a: &(&nucleo::Match, usize, &str, u8), b: &(&nucleo::Match, usize, &str, u8)| {
                        a.3.cmp(&b.3).then(b.0.score.cmp(&a.0.score)).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2))
                    };
                    if limit < ranked.len() {
                        ranked.select_nth_unstable_by(limit, rank);
                        ranked.truncate(limit);
                    }
                    ranked.sort_unstable_by(rank);
                    let matches: Vec<_> = ranked.into_iter().map(|(matched, _, _, _)| matched)
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
                                if !haystack.is_ascii() {
                                    idx_vec = grapheme_indices_to_char_indices(relative_path, &idx_vec);
                                }
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

                    let result = FileSearchSnapshot {
                        query: query.text.clone(),
                        matches,
                        total_match_count: snapshot.matched_item_count() as usize,
                        scanned_file_count: snapshot.item_count() as usize,
                        walk_complete,
                        match_complete: walk_finished && !status.running,
                        cancelled: false,
                    };
                    last_snapshot = result;
                    inner.reporter.on_update(&last_snapshot);
                    metadata_changed = false;
                }
                if !status.running && walk_finished
                    && current_pattern == snapshot_pattern
                    && inner.latest_query.current().generation == query.generation
                    && completed_generation != Some(query.generation)
                {
                    if !last_snapshot.match_complete {
                        last_snapshot.query = query.text.clone();
                        last_snapshot.match_complete = true;
                        inner.reporter.on_update(&last_snapshot);
                    }
                    completed_generation = Some(query.generation);
                    inner.reporter.on_complete_snapshot(&last_snapshot);
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

    let latest = inner.latest_query.current();
    if completed_generation != Some(latest.generation) {
        if last_query
            .as_ref()
            .is_none_or(|query| query.generation != latest.generation)
        {
            last_snapshot = FileSearchSnapshot::default();
        }
        last_snapshot.query = latest.text;
        last_snapshot.match_complete = false;
        last_snapshot.cancelled = true;
        inner.reporter.on_update(&last_snapshot);
        inner.reporter.on_complete_snapshot(&last_snapshot);
    }

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

