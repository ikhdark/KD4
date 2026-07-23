use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::bail;
use clap::ArgAction;
use clap::Parser;
use codex_file_system::open_confined_file;
use ignore::Match;
use ignore::WalkBuilder;
use ignore::gitignore::Gitignore;
use ignore::gitignore::GitignoreBuilder;
use serde::Serialize;
use unicode_casefold::UnicodeCaseFold;

use crate::source_routes::source_map_route_for_path;

pub const SOURCE_SEARCH_DEFAULT_MAX_MATCHES: usize = 100;
pub const SOURCE_SEARCH_MAX_MATCHES: usize = 500;
pub const SOURCE_SEARCH_MAX_CONTEXT_LINES: usize = 5;
pub const SOURCE_SEARCH_MAX_FILES: usize = 2_000;
pub const SOURCE_SEARCH_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const SOURCE_SEARCH_MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const SOURCE_SEARCH_MAX_RESULT_BYTES: usize = 512 * 1024;
pub const SOURCE_SEARCH_MAX_LINE_BYTES: usize = 4 * 1024;
pub const SOURCE_SEARCH_MAX_QUERY_BYTES: usize = 1_024;
pub const SOURCE_SEARCH_MAX_ROOTS: usize = 32;
pub const SOURCE_SEARCH_MAX_WALK_DEPTH: usize = 64;
pub const SOURCE_SEARCH_MAX_WALK_DIRECTORIES: usize = 10_000;
pub const SOURCE_SEARCH_MAX_WALK_ENTRIES: usize = 50_000;
pub const SOURCE_READ_DEFAULT_LINES: usize = 120;
pub const SOURCE_READ_MAX_LINES: usize = 400;
pub const SOURCE_READ_MAX_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy)]
struct SourceWalkLimits {
    max_depth: usize,
    max_directories: usize,
    max_entries: usize,
}

const SOURCE_WALK_LIMITS: SourceWalkLimits = SourceWalkLimits {
    max_depth: SOURCE_SEARCH_MAX_WALK_DEPTH,
    max_directories: SOURCE_SEARCH_MAX_WALK_DIRECTORIES,
    max_entries: SOURCE_SEARCH_MAX_WALK_ENTRIES,
};

#[derive(Clone)]
struct DirectoryIgnoreRules {
    ignore: Gitignore,
    git_ignore: Gitignore,
}

pub struct SourceIgnoreMatcher {
    directory_rules: Mutex<HashMap<PathBuf, DirectoryIgnoreRules>>,
    repository_roots: Mutex<HashMap<PathBuf, Option<PathBuf>>>,
    repository_excludes: Mutex<HashMap<PathBuf, Gitignore>>,
    global_gitignore: Mutex<Gitignore>,
    preloaded: bool,
    preloaded_repository_root: Option<PathBuf>,
}

impl SourceIgnoreMatcher {
    fn new(root: &Path) -> Self {
        let global_base = std::env::current_dir().unwrap_or_else(|_| root.to_path_buf());
        let (global_gitignore, _) = GitignoreBuilder::new(global_base).build_global();
        Self {
            directory_rules: Mutex::new(HashMap::new()),
            repository_roots: Mutex::new(HashMap::new()),
            repository_excludes: Mutex::new(HashMap::new()),
            global_gitignore: Mutex::new(global_gitignore),
            preloaded: false,
            preloaded_repository_root: None,
        }
    }

    /// Creates an ignore matcher whose rule files are supplied by the caller.
    ///
    /// This is used by executor-backed filesystems so ignore files are read
    /// through the selected filesystem and its active sandbox context. Pass
    /// `None` when the search root is not inside a Git repository.
    pub fn new_preloaded(repository_root: Option<&Path>) -> Self {
        Self {
            directory_rules: Mutex::new(HashMap::new()),
            repository_roots: Mutex::new(HashMap::new()),
            repository_excludes: Mutex::new(HashMap::new()),
            global_gitignore: Mutex::new(Gitignore::empty()),
            preloaded: true,
            preloaded_repository_root: repository_root.map(Path::to_path_buf),
        }
    }

    pub fn add_directory_rules(
        &self,
        directory: &Path,
        ignore_contents: Option<&str>,
        git_ignore_contents: Option<&str>,
    ) {
        let rules = DirectoryIgnoreRules {
            ignore: build_ignore_contents_matcher(
                directory,
                &directory.join(".ignore"),
                ignore_contents,
            ),
            git_ignore: build_ignore_contents_matcher(
                directory,
                &directory.join(".gitignore"),
                git_ignore_contents,
            ),
        };
        self.directory_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(directory.to_path_buf(), rules);
    }

    pub fn has_directory_rules(&self, directory: &Path) -> bool {
        self.directory_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(directory)
    }

    pub fn set_repository_exclude(
        &self,
        repository_root: &Path,
        source_path: &Path,
        contents: &str,
    ) {
        self.repository_excludes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                repository_root.to_path_buf(),
                build_ignore_contents_matcher(repository_root, source_path, Some(contents)),
            );
    }

    pub fn set_global_gitignore(&self, base: &Path, source_path: &Path, contents: &str) {
        *self
            .global_gitignore
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            build_ignore_contents_matcher(base, source_path, Some(contents));
    }

    pub fn is_ignored(&self, path: &Path, is_directory: bool) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };

        for directory in parent.ancestors() {
            let rules = self.rules_for(directory);
            if let Some(ignored) = ignore_decision(rules.ignore.matched(path, is_directory)) {
                return ignored;
            }
        }

        let Some(repository_root) = self.repository_root_for(parent) else {
            return false;
        };
        for directory in parent.ancestors() {
            let rules = self.rules_for(directory);
            if let Some(ignored) = ignore_decision(rules.git_ignore.matched(path, is_directory)) {
                return ignored;
            }
            if directory == repository_root {
                break;
            }
        }

        let exclude = self.repository_exclude_for(&repository_root);
        if let Some(ignored) = ignore_decision(exclude.matched(path, is_directory)) {
            return ignored;
        }
        ignore_decision(
            self.global_gitignore
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .matched(path, is_directory),
        )
        .unwrap_or(false)
    }

    fn rules_for(&self, directory: &Path) -> DirectoryIgnoreRules {
        let mut cache = self
            .directory_rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.preloaded {
            return cache
                .get(directory)
                .cloned()
                .unwrap_or_else(DirectoryIgnoreRules::empty);
        }
        cache
            .entry(directory.to_path_buf())
            .or_insert_with(|| DirectoryIgnoreRules {
                ignore: build_ignore_file_matcher(directory, &directory.join(".ignore")),
                git_ignore: build_ignore_file_matcher(directory, &directory.join(".gitignore")),
            })
            .clone()
    }

    fn repository_root_for(&self, directory: &Path) -> Option<PathBuf> {
        if self.preloaded {
            return self
                .preloaded_repository_root
                .as_ref()
                .filter(|repository_root| directory.starts_with(repository_root))
                .cloned();
        }
        let mut cache = self
            .repository_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .entry(directory.to_path_buf())
            .or_insert_with(|| {
                directory
                    .ancestors()
                    .find(|ancestor| ancestor.join(".git").metadata().is_ok())
                    .map(Path::to_path_buf)
            })
            .clone()
    }

    fn repository_exclude_for(&self, repository_root: &Path) -> Gitignore {
        let mut cache = self
            .repository_excludes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.preloaded {
            return cache
                .get(repository_root)
                .cloned()
                .unwrap_or_else(Gitignore::empty);
        }
        cache
            .entry(repository_root.to_path_buf())
            .or_insert_with(|| {
                let git_dir = resolve_git_common_directory(repository_root)
                    .unwrap_or_else(|| repository_root.join(".git"));
                build_ignore_file_matcher(repository_root, &git_dir.join("info/exclude"))
            })
            .clone()
    }
}

impl DirectoryIgnoreRules {
    fn empty() -> Self {
        Self {
            ignore: Gitignore::empty(),
            git_ignore: Gitignore::empty(),
        }
    }
}

fn build_ignore_file_matcher(root: &Path, ignore_file: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let _ = builder.add(ignore_file);
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn build_ignore_contents_matcher(
    root: &Path,
    source_path: &Path,
    contents: Option<&str>,
) -> Gitignore {
    let Some(contents) = contents else {
        return Gitignore::empty();
    };
    let mut builder = GitignoreBuilder::new(root);
    for (index, line) in contents.lines().enumerate() {
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        let _ = builder.add_line(Some(source_path.to_path_buf()), line);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn resolve_git_common_directory(repository_root: &Path) -> Option<PathBuf> {
    let dot_git = repository_root.join(".git");
    let metadata = dot_git.metadata().ok()?;
    if !metadata.is_file() {
        return Some(dot_git);
    }

    let dot_git_contents = fs::read_to_string(dot_git).ok()?;
    let git_dir_target = dot_git_contents.strip_prefix("gitdir:")?.trim();
    if git_dir_target.is_empty() {
        return None;
    }
    let real_git_dir = PathBuf::from(git_dir_target);
    let real_git_dir = if real_git_dir.is_absolute() {
        real_git_dir
    } else {
        repository_root.join(real_git_dir)
    };
    let common_dir = fs::read_to_string(real_git_dir.join("commondir"))
        .ok()
        .map(|contents| contents.trim().to_owned())
        .filter(|contents| !contents.is_empty())
        .map(PathBuf::from)
        .map(|common_dir| {
            if common_dir.is_absolute() {
                common_dir
            } else {
                real_git_dir.join(common_dir)
            }
        })
        .unwrap_or(real_git_dir);
    Some(common_dir)
}

fn ignore_decision<T>(matched: Match<T>) -> Option<bool> {
    match matched {
        Match::None => None,
        Match::Ignore(_) => Some(true),
        Match::Whitelist(_) => Some(false),
    }
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Bounded fixed-string source search and confined source-span reads."
)]
pub struct SourceSearchCli {
    /// Fixed string to search for. Omit when using --read-file.
    pub query: Option<String>,

    /// Repository root that confines every search and read path.
    #[arg(long, default_value = ".")]
    pub repo_root: PathBuf,

    /// File or directory to search. Repeat to search multiple confined roots.
    #[arg(long = "path", value_name = "PATH", action = ArgAction::Append)]
    pub roots: Vec<PathBuf>,

    /// Read a bounded line span instead of searching.
    #[arg(long, value_name = "PATH")]
    pub read_file: Option<PathBuf>,

    /// First 1-based line for --read-file.
    #[arg(long)]
    pub start_line: Option<usize>,

    /// Number of lines for --read-file.
    #[arg(long)]
    pub line_count: Option<usize>,

    /// Maximum number of search matches to return.
    #[arg(long)]
    pub max_matches: Option<usize>,

    /// Context lines around each search match.
    #[arg(long)]
    pub context_lines: Option<usize>,

    /// Use case-sensitive fixed-string matching.
    #[arg(long)]
    pub case_sensitive: bool,

    /// Include generated-looking paths.
    #[arg(long)]
    pub include_generated: bool,

    /// Include vendored dependency paths.
    #[arg(long)]
    pub include_vendor: bool,

    /// Include lockfiles.
    #[arg(long)]
    pub include_locks: bool,

    /// Emit structured JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct SourceSearchOptions {
    pub repo_root: PathBuf,
    pub roots: Vec<PathBuf>,
    pub query: String,
    pub max_matches: usize,
    pub context_lines: usize,
    pub case_sensitive: bool,
    pub include_generated: bool,
    pub include_vendor: bool,
    pub include_locks: bool,
}

impl SourceSearchOptions {
    pub fn new(repo_root: PathBuf, query: String) -> Self {
        Self {
            repo_root,
            roots: Vec::new(),
            query,
            max_matches: SOURCE_SEARCH_DEFAULT_MAX_MATCHES,
            context_lines: 0,
            case_sensitive: false,
            include_generated: false,
            include_vendor: false,
            include_locks: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadFileSpanOptions {
    pub repo_root: PathBuf,
    pub path: PathBuf,
    pub start_line: usize,
    pub line_count: usize,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct SourceSearchOutput {
    pub query: String,
    pub roots: Vec<String>,
    pub truncated: bool,
    pub truncated_reason: Option<SourceTruncatedReason>,
    pub coverage: SourceSearchCoverage,
    pub matches: Vec<SourceSearchMatch>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct SourceSearchCoverage {
    pub files_scanned: usize,
    pub files_skipped_too_large: usize,
    pub files_skipped_non_utf8: usize,
    pub files_changed_during_read: usize,
    pub filesystem_errors: usize,
    pub bytes_scanned: usize,
    pub result_bytes: usize,
    pub total_matches: usize,
    pub matches_returned: usize,
    pub max_matches: usize,
    pub max_files: usize,
    pub max_bytes: usize,
    pub max_file_bytes: usize,
    pub max_result_bytes: usize,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceTruncatedReason {
    MaxMatches,
    MaxFiles,
    MaxBytes,
    MaxResultBytes,
    WalkLimit,
    FilesChangedDuringRead,
    OversizedFiles,
    NonUtf8Files,
    FilesystemErrors,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct SourceSearchMatch {
    pub path: String,
    pub source_map_route: Option<String>,
    pub line_number: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub lines: Vec<SourceLine>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct SourceLine {
    pub line_number: usize,
    pub text: String,
    pub text_truncated: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct ReadFileSpanOutput {
    pub path: String,
    pub source_map_route: Option<String>,
    pub requested_start_line: usize,
    pub requested_line_count: usize,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub total_lines: usize,
    pub bytes_returned: usize,
    pub truncated: bool,
    pub lines: Vec<SourceLine>,
}

pub fn run_source_search_cli(cli: SourceSearchCli) -> anyhow::Result<()> {
    let json = cli.json;
    if let Some(path) = cli.read_file {
        if cli.query.is_some()
            || !cli.roots.is_empty()
            || cli.max_matches.is_some()
            || cli.context_lines.is_some()
            || cli.case_sensitive
            || cli.include_generated
            || cli.include_vendor
            || cli.include_locks
        {
            bail!("--read-file cannot be combined with search-only arguments");
        }
        let start_line = cli.start_line.unwrap_or(1);
        if start_line == 0 {
            bail!("--start-line must be 1 or greater");
        }
        let line_count = cli.line_count.unwrap_or(SOURCE_READ_DEFAULT_LINES);
        if !(1..=SOURCE_READ_MAX_LINES).contains(&line_count) {
            bail!(
                "--line-count must be between 1 and {SOURCE_READ_MAX_LINES} (received {line_count})"
            );
        }
        let output = read_file_span(ReadFileSpanOptions {
            repo_root: cli.repo_root,
            path,
            start_line,
            line_count,
        })?;
        print_output(&output, json, print_span_human)
    } else {
        if cli.start_line.is_some() || cli.line_count.is_some() {
            bail!("--start-line and --line-count require --read-file");
        }
        let Some(query) = cli.query else {
            bail!("a query or --read-file is required");
        };
        let max_matches = cli.max_matches.unwrap_or(SOURCE_SEARCH_DEFAULT_MAX_MATCHES);
        if !(1..=SOURCE_SEARCH_MAX_MATCHES).contains(&max_matches) {
            bail!(
                "--max-matches must be between 1 and {SOURCE_SEARCH_MAX_MATCHES} (received {max_matches})"
            );
        }
        let context_lines = cli.context_lines.unwrap_or(0);
        if context_lines > SOURCE_SEARCH_MAX_CONTEXT_LINES {
            bail!(
                "--context-lines must not exceed {SOURCE_SEARCH_MAX_CONTEXT_LINES} (received {context_lines})"
            );
        }
        let mut options = SourceSearchOptions::new(cli.repo_root, query);
        options.roots = cli.roots;
        options.max_matches = max_matches;
        options.context_lines = context_lines;
        options.case_sensitive = cli.case_sensitive;
        options.include_generated = cli.include_generated;
        options.include_vendor = cli.include_vendor;
        options.include_locks = cli.include_locks;
        let output = search_source(options)?;
        print_output(&output, json, print_search_human)
    }
}

pub fn search_source(options: SourceSearchOptions) -> anyhow::Result<SourceSearchOutput> {
    search_source_with_walk_limits(options, SOURCE_WALK_LIMITS)
}

fn search_source_with_walk_limits(
    options: SourceSearchOptions,
    walk_limits: SourceWalkLimits,
) -> anyhow::Result<SourceSearchOutput> {
    let repo_root = canonical_repo_root(&options.repo_root)?;
    let roots = resolve_search_roots(&repo_root, &options.roots)?;
    let mut accumulator = SourceSearchAccumulator::new(&options)?;

    for root in &roots {
        if accumulator.should_stop() {
            break;
        }
        scan_root(&repo_root, root, &mut accumulator, walk_limits)?;
    }

    let roots = roots
        .iter()
        .map(|root| relative_display(&repo_root, root))
        .collect();
    Ok(accumulator.finish(roots))
}

pub fn read_file_span(options: ReadFileSpanOptions) -> anyhow::Result<ReadFileSpanOutput> {
    validate_read_file_span_bounds(options.start_line, options.line_count)?;
    let repo_root = canonical_repo_root(&options.repo_root)?;
    let path = resolve_confined_path(&repo_root, &options.path, "source file")?;
    let mut file = open_confined_file(&repo_root, &path)
        .with_context(|| format!("unable to open source file `{}`", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("unable to inspect source file `{}`", path.display()))?;
    if !metadata.is_file() {
        bail!("source path `{}` is not a file", options.path.display());
    }
    let file_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if file_len > SOURCE_SEARCH_MAX_FILE_BYTES {
        bail!(
            "source file `{}` is too large ({} bytes, max {})",
            options.path.display(),
            file_len,
            SOURCE_SEARCH_MAX_FILE_BYTES
        );
    }

    let Some(bytes) = read_open_file_stably(&mut file, &repo_root, &path, &metadata)? else {
        bail!(
            "source file `{}` changed while it was being read; retry the read",
            options.path.display()
        );
    };
    let relative_path = relative_display(&repo_root, &path);
    read_file_span_from_bytes(relative_path, bytes, options.start_line, options.line_count)
}

/// Builds a bounded line-span result from bytes supplied by the caller's filesystem.
pub fn read_file_span_from_bytes(
    relative_path: String,
    bytes: Vec<u8>,
    start_line: usize,
    line_count: usize,
) -> anyhow::Result<ReadFileSpanOutput> {
    validate_read_file_span_bounds(start_line, line_count)?;
    if bytes.len() > SOURCE_SEARCH_MAX_FILE_BYTES {
        bail!(
            "source file `{relative_path}` is too large ({} bytes, max {})",
            bytes.len(),
            SOURCE_SEARCH_MAX_FILE_BYTES
        );
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("source file `{relative_path}` is not UTF-8"))?;
    let source_lines = text.lines().collect::<Vec<_>>();
    let start_index = start_line.saturating_sub(1).min(source_lines.len());
    let end_index = start_index
        .saturating_add(line_count)
        .min(source_lines.len());
    let mut lines = Vec::new();
    let mut bytes_returned = 0usize;
    let mut byte_truncated = false;

    for (offset, text) in source_lines[start_index..end_index].iter().enumerate() {
        let remaining = SOURCE_READ_MAX_BYTES.saturating_sub(bytes_returned);
        if remaining == 0 {
            byte_truncated = true;
            break;
        }
        let (text, text_truncated) = bounded_text(text, remaining);
        bytes_returned = bytes_returned.saturating_add(text.len());
        lines.push(SourceLine {
            line_number: start_index + offset + 1,
            text,
            text_truncated,
        });
        if text_truncated {
            byte_truncated = true;
            break;
        }
    }

    Ok(ReadFileSpanOutput {
        path: relative_path.clone(),
        source_map_route: source_map_route_for_path(Path::new(&relative_path)),
        requested_start_line: start_line,
        requested_line_count: line_count,
        start_line: lines.first().map(|line| line.line_number),
        end_line: lines.last().map(|line| line.line_number),
        total_lines: source_lines.len(),
        bytes_returned,
        truncated: byte_truncated,
        lines,
    })
}

pub fn validate_read_file_span_bounds(start_line: usize, line_count: usize) -> anyhow::Result<()> {
    if start_line == 0 {
        bail!("start_line must be 1 or greater");
    }
    if !(1..=SOURCE_READ_MAX_LINES).contains(&line_count) {
        bail!("line_count must be between 1 and {SOURCE_READ_MAX_LINES} (received {line_count})");
    }
    Ok(())
}

pub struct SourceSearchAccumulator {
    query: String,
    query_cmp: String,
    case_sensitive: bool,
    context_lines: usize,
    include_generated: bool,
    include_vendor: bool,
    include_locks: bool,
    state: SearchState,
}

impl SourceSearchAccumulator {
    pub fn new(options: &SourceSearchOptions) -> anyhow::Result<Self> {
        validate_query(&options.query)?;
        if !(1..=SOURCE_SEARCH_MAX_MATCHES).contains(&options.max_matches) {
            bail!(
                "max_matches must be between 1 and {SOURCE_SEARCH_MAX_MATCHES} (received {})",
                options.max_matches
            );
        }
        if options.context_lines > SOURCE_SEARCH_MAX_CONTEXT_LINES {
            bail!(
                "context_lines must not exceed {SOURCE_SEARCH_MAX_CONTEXT_LINES} (received {})",
                options.context_lines
            );
        }
        let query_cmp = if options.case_sensitive {
            options.query.clone()
        } else {
            unicode_case_fold(&options.query)
        };
        Ok(Self {
            query: options.query.clone(),
            query_cmp,
            case_sensitive: options.case_sensitive,
            context_lines: options.context_lines,
            include_generated: options.include_generated,
            include_vendor: options.include_vendor,
            include_locks: options.include_locks,
            state: SearchState::new(options.max_matches),
        })
    }

    pub fn should_stop(&self) -> bool {
        self.state.coverage_limit.is_some()
    }

    /// Records a candidate source file and returns whether its bytes should be read.
    pub fn consider_file(&mut self, relative_path: &Path, file_len: usize) -> bool {
        if !should_scan_source_file(
            relative_path,
            self.include_generated,
            self.include_vendor,
            self.include_locks,
        ) {
            return false;
        }
        if self.state.files_scanned >= SOURCE_SEARCH_MAX_FILES {
            self.state.coverage_limit = Some(SourceTruncatedReason::MaxFiles);
            return false;
        }
        self.state.files_scanned = self.state.files_scanned.saturating_add(1);
        if file_len > SOURCE_SEARCH_MAX_FILE_BYTES {
            self.state.files_skipped_too_large =
                self.state.files_skipped_too_large.saturating_add(1);
            return false;
        }
        if self.state.bytes_scanned.saturating_add(file_len) > SOURCE_SEARCH_MAX_BYTES {
            self.state.coverage_limit = Some(SourceTruncatedReason::MaxBytes);
            return false;
        }
        true
    }

    /// Adds bytes obtained through the caller's filesystem abstraction.
    pub fn add_file_bytes(&mut self, relative_path: &Path, bytes: Vec<u8>) {
        if bytes.len() > SOURCE_SEARCH_MAX_FILE_BYTES {
            self.state.files_skipped_too_large =
                self.state.files_skipped_too_large.saturating_add(1);
            return;
        }
        if self.state.bytes_scanned.saturating_add(bytes.len()) > SOURCE_SEARCH_MAX_BYTES {
            self.state.coverage_limit = Some(SourceTruncatedReason::MaxBytes);
            return;
        }
        self.state.bytes_scanned = self.state.bytes_scanned.saturating_add(bytes.len());
        let Ok(text) = String::from_utf8(bytes) else {
            self.state.files_skipped_non_utf8 = self.state.files_skipped_non_utf8.saturating_add(1);
            return;
        };
        collect_matches(
            relative_path,
            &text,
            self.case_sensitive,
            self.context_lines,
            &self.query_cmp,
            &mut self.state,
        );
    }

    pub fn mark_walk_limit(&mut self) {
        self.state
            .coverage_limit
            .get_or_insert(SourceTruncatedReason::WalkLimit);
    }

    pub fn reserve_walk_directory(&mut self, limit: usize) -> bool {
        if self.state.walk_directories_seen >= limit {
            self.mark_walk_limit();
            return false;
        }
        self.state.walk_directories_seen = self.state.walk_directories_seen.saturating_add(1);
        true
    }

    pub fn remaining_walk_entries(&self, limit: usize) -> usize {
        limit.saturating_sub(self.state.walk_entries_seen)
    }

    pub fn record_walk_entries(&mut self, count: usize, limit: usize) {
        self.state.walk_entries_seen = self
            .state
            .walk_entries_seen
            .saturating_add(count)
            .min(limit);
    }

    pub fn mark_file_changed_during_read(&mut self) {
        self.state.files_changed_during_read =
            self.state.files_changed_during_read.saturating_add(1);
    }

    pub fn mark_filesystem_error(&mut self) {
        self.state.filesystem_errors = self.state.filesystem_errors.saturating_add(1);
    }

    pub fn finish(mut self, roots: Vec<String>) -> SourceSearchOutput {
        self.state.matches.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.line_number.cmp(&right.line_number))
        });
        let coverage_limit = self.state.coverage_limit;
        let mut result_limit = self.state.result_limit;
        let coverage = SourceSearchCoverage {
            files_scanned: self.state.files_scanned,
            files_skipped_too_large: self.state.files_skipped_too_large,
            files_skipped_non_utf8: self.state.files_skipped_non_utf8,
            files_changed_during_read: self.state.files_changed_during_read,
            filesystem_errors: self.state.filesystem_errors,
            bytes_scanned: self.state.bytes_scanned,
            result_bytes: self.state.result_bytes,
            total_matches: self.state.total_matches,
            matches_returned: self.state.matches.len(),
            max_matches: self.state.max_matches,
            max_files: SOURCE_SEARCH_MAX_FILES,
            max_bytes: SOURCE_SEARCH_MAX_BYTES,
            max_file_bytes: SOURCE_SEARCH_MAX_FILE_BYTES,
            max_result_bytes: SOURCE_SEARCH_MAX_RESULT_BYTES,
        };
        let mut output = SourceSearchOutput {
            query: self.query,
            roots,
            truncated: false,
            truncated_reason: None,
            coverage,
            matches: self.state.matches,
        };

        loop {
            output.truncated_reason = source_truncated_reason(
                coverage_limit,
                result_limit,
                output.coverage.files_changed_during_read,
                output.coverage.filesystem_errors,
                output.coverage.files_skipped_too_large,
                output.coverage.files_skipped_non_utf8,
            );
            output.truncated = output.truncated_reason.is_some();
            output.coverage.matches_returned = output.matches.len();
            let serialized_bytes = refresh_serialized_result_bytes(&mut output);
            if serialized_bytes <= SOURCE_SEARCH_MAX_RESULT_BYTES {
                return output;
            }

            result_limit = Some(SourceTruncatedReason::MaxResultBytes);
            if output.matches.pop().is_none() {
                output.truncated_reason = source_truncated_reason(
                    coverage_limit,
                    result_limit,
                    output.coverage.files_changed_during_read,
                    output.coverage.filesystem_errors,
                    output.coverage.files_skipped_too_large,
                    output.coverage.files_skipped_non_utf8,
                );
                output.truncated = true;
                output.coverage.matches_returned = 0;
                refresh_serialized_result_bytes(&mut output);
                return output;
            }
        }
    }
}

struct SearchState {
    walk_directories_seen: usize,
    walk_entries_seen: usize,
    files_scanned: usize,
    files_skipped_too_large: usize,
    files_skipped_non_utf8: usize,
    files_changed_during_read: usize,
    filesystem_errors: usize,
    bytes_scanned: usize,
    result_bytes: usize,
    total_matches: usize,
    max_matches: usize,
    coverage_limit: Option<SourceTruncatedReason>,
    result_limit: Option<SourceTruncatedReason>,
    matches_serialized_bytes: usize,
    matches: Vec<SourceSearchMatch>,
}

impl SearchState {
    fn new(max_matches: usize) -> Self {
        Self {
            walk_directories_seen: 0,
            walk_entries_seen: 0,
            files_scanned: 0,
            files_skipped_too_large: 0,
            files_skipped_non_utf8: 0,
            files_changed_during_read: 0,
            filesystem_errors: 0,
            bytes_scanned: 0,
            result_bytes: 0,
            total_matches: 0,
            max_matches,
            coverage_limit: None,
            result_limit: None,
            matches_serialized_bytes: 2,
            matches: Vec::new(),
        }
    }
}

fn source_truncated_reason(
    coverage_limit: Option<SourceTruncatedReason>,
    result_limit: Option<SourceTruncatedReason>,
    files_changed_during_read: usize,
    filesystem_errors: usize,
    files_skipped_too_large: usize,
    files_skipped_non_utf8: usize,
) -> Option<SourceTruncatedReason> {
    coverage_limit
        .or(result_limit)
        .or_else(|| {
            (files_changed_during_read > 0).then_some(SourceTruncatedReason::FilesChangedDuringRead)
        })
        .or_else(|| (filesystem_errors > 0).then_some(SourceTruncatedReason::FilesystemErrors))
        .or_else(|| (files_skipped_too_large > 0).then_some(SourceTruncatedReason::OversizedFiles))
        .or_else(|| (files_skipped_non_utf8 > 0).then_some(SourceTruncatedReason::NonUtf8Files))
}

fn refresh_serialized_result_bytes(output: &mut SourceSearchOutput) -> usize {
    loop {
        let serialized_bytes = serde_json::to_vec_pretty(output)
            .map(|bytes| bytes.len().saturating_add(1))
            .unwrap_or(usize::MAX);
        if output.coverage.result_bytes == serialized_bytes {
            return serialized_bytes;
        }
        output.coverage.result_bytes = serialized_bytes;
    }
}

fn scan_root(
    repo_root: &Path,
    root: &Path,
    accumulator: &mut SourceSearchAccumulator,
    walk_limits: SourceWalkLimits,
) -> anyhow::Result<()> {
    let metadata = match fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(_) => {
            accumulator.mark_filesystem_error();
            return Ok(());
        }
    };
    if metadata.is_file() {
        recover_scan_result(scan_file(repo_root, root, accumulator), accumulator);
        return Ok(());
    }
    if !metadata.is_dir() {
        bail!(
            "source root `{}` is neither a file nor a directory",
            root.display()
        );
    }

    let include_generated = accumulator.include_generated;
    let include_vendor = accumulator.include_vendor;
    let depth_limit_hit = Arc::new(AtomicBool::new(false));
    let filter_depth_limit_hit = Arc::clone(&depth_limit_hit);
    let remaining_entries = accumulator.remaining_walk_entries(walk_limits.max_entries);
    if remaining_entries == 0 {
        accumulator.mark_walk_limit();
        return Ok(());
    }
    let entries_examined = Arc::new(AtomicUsize::new(0));
    let filter_entries_examined = Arc::clone(&entries_examined);
    let entry_limit_hit = Arc::new(AtomicBool::new(false));
    let filter_entry_limit_hit = Arc::clone(&entry_limit_hit);
    let ignore_matcher = Arc::new(SourceIgnoreMatcher::new(root));
    let filter_ignore_matcher = Arc::clone(&ignore_matcher);
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .max_depth(Some(walk_limits.max_depth.saturating_add(1)))
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let examined = filter_entries_examined
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            if examined >= remaining_entries {
                filter_entry_limit_hit.store(true, Ordering::Relaxed);
                // Force the budget-consuming entry to be yielded so the
                // iterator can be dropped before it examines another entry.
                return true;
            }
            let (should_yield, depth_exceeded) =
                source_walk_entry_filter(entry, walk_limits, include_generated, include_vendor);
            if depth_exceeded {
                filter_depth_limit_hit.store(true, Ordering::Relaxed);
            }
            let is_directory = entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir());
            should_yield && !filter_ignore_matcher.is_ignored(entry.path(), is_directory)
        });

    for entry in builder.build() {
        if depth_limit_hit.load(Ordering::Relaxed) {
            accumulator.mark_walk_limit();
            break;
        }
        if accumulator.should_stop() {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let _ = recover_walk_entry::<ignore::DirEntry, _>(Err(error), accumulator);
                let examined = entries_examined
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                if examined >= remaining_entries {
                    entry_limit_hit.store(true, Ordering::Relaxed);
                    accumulator.mark_walk_limit();
                    break;
                }
                continue;
            }
        };
        let (mut should_process, depth_exceeded) =
            source_walk_entry_filter(&entry, walk_limits, include_generated, include_vendor);
        if depth_exceeded {
            depth_limit_hit.store(true, Ordering::Relaxed);
        }
        let is_directory = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
        if should_process && ignore_matcher.is_ignored(entry.path(), is_directory) {
            should_process = false;
        }
        if is_directory {
            if should_process && !accumulator.reserve_walk_directory(walk_limits.max_directories) {
                break;
            }
            if entry_limit_hit.load(Ordering::Relaxed) {
                accumulator.mark_walk_limit();
                break;
            }
            continue;
        }
        if should_process
            && entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        {
            recover_scan_result(scan_file(repo_root, entry.path(), accumulator), accumulator);
        }
        if entry_limit_hit.load(Ordering::Relaxed) {
            accumulator.mark_walk_limit();
            break;
        }
    }
    accumulator.record_walk_entries(
        entries_examined
            .load(Ordering::Relaxed)
            .min(remaining_entries),
        walk_limits.max_entries,
    );
    if depth_limit_hit.load(Ordering::Relaxed) {
        accumulator.mark_walk_limit();
    }
    if entry_limit_hit.load(Ordering::Relaxed) {
        accumulator.mark_walk_limit();
    }
    Ok(())
}

fn source_walk_entry_filter(
    entry: &ignore::DirEntry,
    walk_limits: SourceWalkLimits,
    include_generated: bool,
    include_vendor: bool,
) -> (bool, bool) {
    let is_directory = entry
        .file_type()
        .is_some_and(|file_type| file_type.is_dir());
    if entry.depth() > 0
        && is_directory
        && !should_descend_source_path(entry.path(), include_generated, include_vendor)
    {
        return (false, false);
    }
    let max_entry_depth = walk_limits
        .max_depth
        .saturating_add(usize::from(!is_directory));
    if entry.depth() > max_entry_depth {
        return (false, true);
    }
    (true, false)
}

fn recover_walk_entry<T, E>(
    entry: Result<T, E>,
    accumulator: &mut SourceSearchAccumulator,
) -> Option<T> {
    match entry {
        Ok(entry) => Some(entry),
        Err(_) => {
            accumulator.mark_filesystem_error();
            None
        }
    }
}

fn recover_scan_result(result: anyhow::Result<()>, accumulator: &mut SourceSearchAccumulator) {
    if result.is_err() {
        accumulator.mark_filesystem_error();
    }
}

fn scan_file(
    repo_root: &Path,
    path: &Path,
    accumulator: &mut SourceSearchAccumulator,
) -> anyhow::Result<()> {
    let path = resolve_confined_path(repo_root, path, "source file")?;
    let mut file = open_confined_file(repo_root, &path)?;
    let metadata = file.metadata()?;
    let file_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let relative_path = path.strip_prefix(repo_root).unwrap_or(&path);
    if !accumulator.consider_file(relative_path, file_len) {
        return Ok(());
    }

    match read_open_file_stably(&mut file, repo_root, &path, &metadata)? {
        Some(bytes) => accumulator.add_file_bytes(relative_path, bytes),
        None => accumulator.mark_file_changed_during_read(),
    }
    Ok(())
}

fn read_open_file_stably(
    file: &mut File,
    repo_root: &Path,
    path: &Path,
    metadata_before: &fs::Metadata,
) -> anyhow::Result<Option<Vec<u8>>> {
    let identity_before = native_file_identity(file, metadata_before)
        .with_context(|| format!("unable to identify source file `{}`", path.display()))?;
    let Some(bytes) = read_open_file_once(file, path, metadata_before)? else {
        return Ok(None);
    };
    let mut verification_file = match open_confined_file(repo_root, path) {
        Ok(file) => file,
        Err(err) if is_changed_file_race_error(err.kind()) => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("unable to reopen source file `{}`", path.display()));
        }
    };
    let verification_metadata = verification_file
        .metadata()
        .with_context(|| format!("unable to re-inspect source file `{}`", path.display()))?;
    let verification_identity = native_file_identity(&verification_file, &verification_metadata)
        .with_context(|| format!("unable to re-identify source file `{}`", path.display()))?;
    if !verification_metadata.is_file()
        || file_metadata_changed(metadata_before, &verification_metadata)
        || identity_before != verification_identity
    {
        return Ok(None);
    }
    let Some(verification_bytes) =
        read_open_file_once(&mut verification_file, path, &verification_metadata)?
    else {
        return Ok(None);
    };
    if bytes != verification_bytes {
        return Ok(None);
    }
    let final_file = match open_confined_file(repo_root, path) {
        Ok(file) => file,
        Err(err) if is_changed_file_race_error(err.kind()) => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "unable to reopen source file `{}` after reading",
                    path.display()
                )
            });
        }
    };
    let final_metadata = final_file
        .metadata()
        .with_context(|| format!("unable to finally inspect source file `{}`", path.display()))?;
    let final_identity = native_file_identity(&final_file, &final_metadata).with_context(|| {
        format!(
            "unable to finally identify source file `{}`",
            path.display()
        )
    })?;
    if file_metadata_changed(&verification_metadata, &final_metadata)
        || verification_identity != final_identity
    {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn read_open_file_once(
    file: &mut File,
    path: &Path,
    metadata_before: &fs::Metadata,
) -> anyhow::Result<Option<Vec<u8>>> {
    let expected_len = usize::try_from(metadata_before.len()).unwrap_or(usize::MAX);
    let read_limit = SOURCE_SEARCH_MAX_FILE_BYTES.saturating_add(1);
    let mut bytes = Vec::with_capacity(expected_len.min(SOURCE_SEARCH_MAX_FILE_BYTES));
    Read::by_ref(file)
        .take(read_limit as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("unable to read source file `{}`", path.display()))?;
    let metadata_after = file
        .metadata()
        .with_context(|| format!("unable to re-inspect source file `{}`", path.display()))?;
    if bytes.len() != expected_len || file_metadata_changed(metadata_before, &metadata_after) {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn file_metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before.created().ok() != after.created().ok()
        || before.is_file() != after.is_file()
        || before.is_dir() != after.is_dir()
        || before.is_symlink() != after.is_symlink()
        || platform_file_metadata_changed(before, after)
}

#[cfg(unix)]
fn platform_file_metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
}

#[cfg(windows)]
fn platform_file_metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    before.file_attributes() != after.file_attributes()
        || before.creation_time() != after.creation_time()
        || before.last_write_time() != after.last_write_time()
}

#[cfg(not(any(unix, windows)))]
fn platform_file_metadata_changed(_before: &fs::Metadata, _after: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn native_file_identity(_file: &File, metadata: &fs::Metadata) -> io::Result<NativeFileIdentity> {
    use std::os::unix::fs::MetadataExt;

    Ok(NativeFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeFileIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

#[cfg(windows)]
fn native_file_identity(file: &File, _metadata: &fs::Metadata) -> io::Result<NativeFileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `file` owns a valid handle and `information` points to writable,
    // correctly sized storage for the duration of the call.
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as HANDLE, information.as_mut_ptr())
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful call initialized the complete structure.
    let information = unsafe { information.assume_init() };
    Ok(NativeFileIdentity {
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeFileIdentity;

#[cfg(not(any(unix, windows)))]
fn native_file_identity(_file: &File, _metadata: &fs::Metadata) -> io::Result<NativeFileIdentity> {
    Ok(NativeFileIdentity)
}

fn is_changed_file_race_error(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::InvalidInput
    )
}

fn collect_matches(
    relative_path: &Path,
    text: &str,
    case_sensitive: bool,
    context_lines: usize,
    query_cmp: &str,
    state: &mut SearchState,
) {
    let lines = text.lines().collect::<Vec<_>>();
    let relative_path = relative_path.to_string_lossy().replace('\\', "/");
    for (index, line) in lines.iter().enumerate() {
        let is_match = if case_sensitive {
            line.contains(query_cmp)
        } else {
            unicode_case_fold(line).contains(query_cmp)
        };
        if !is_match {
            continue;
        }
        state.total_matches = state.total_matches.saturating_add(1);
        if state.matches.len() >= state.max_matches {
            state
                .result_limit
                .get_or_insert(SourceTruncatedReason::MaxMatches);
            continue;
        }
        let start = index.saturating_sub(context_lines);
        let end = index
            .saturating_add(context_lines)
            .saturating_add(1)
            .min(lines.len());
        let source_lines = lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, text)| {
                let (text, text_truncated) = bounded_text(text, SOURCE_SEARCH_MAX_LINE_BYTES);
                SourceLine {
                    line_number: start + offset + 1,
                    text,
                    text_truncated,
                }
            })
            .collect::<Vec<_>>();
        let source_match = SourceSearchMatch {
            path: relative_path.clone(),
            source_map_route: source_map_route_for_path(Path::new(&relative_path)),
            line_number: index + 1,
            start_line: start + 1,
            end_line: end,
            lines: source_lines,
        };
        let serialized_match_bytes = serde_json::to_vec(&source_match)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        let separator_bytes = usize::from(!state.matches.is_empty());
        let result_bytes = state
            .matches_serialized_bytes
            .saturating_add(separator_bytes)
            .saturating_add(serialized_match_bytes);
        if result_bytes > SOURCE_SEARCH_MAX_RESULT_BYTES {
            state
                .result_limit
                .get_or_insert(SourceTruncatedReason::MaxResultBytes);
            continue;
        }
        state.matches.push(source_match);
        state.matches_serialized_bytes = result_bytes;
        state.result_bytes = result_bytes;
    }
}

fn unicode_case_fold(value: &str) -> String {
    value.case_fold().collect()
}

fn canonical_repo_root(repo_root: &Path) -> anyhow::Result<PathBuf> {
    let repo_root = fs::canonicalize(repo_root)
        .with_context(|| format!("repository root `{}` does not exist", repo_root.display()))?;
    if !repo_root.is_dir() {
        bail!(
            "repository root `{}` is not a directory",
            repo_root.display()
        );
    }
    Ok(repo_root)
}

fn resolve_search_roots(repo_root: &Path, roots: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    if roots.len() > SOURCE_SEARCH_MAX_ROOTS {
        bail!(
            "too many source roots ({} provided, max {})",
            roots.len(),
            SOURCE_SEARCH_MAX_ROOTS
        );
    }
    let roots = if roots.is_empty() {
        vec![repo_root.to_path_buf()]
    } else {
        roots
            .iter()
            .map(|root| resolve_confined_path(repo_root, root, "source root"))
            .collect::<anyhow::Result<Vec<_>>>()?
    };
    let mut roots = roots;
    roots.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    roots.dedup();

    let mut deduped = Vec::<PathBuf>::new();
    for root in roots {
        if deduped.iter().any(|parent| root.starts_with(parent)) {
            continue;
        }
        deduped.push(root);
    }
    Ok(deduped)
}

fn resolve_confined_path(repo_root: &Path, path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };
    let canonical = fs::canonicalize(&candidate)
        .with_context(|| format!("{label} `{}` does not exist", path.display()))?;
    if !canonical.starts_with(repo_root) {
        bail!(
            "{label} `{}` resolves outside repository root `{}`",
            path.display(),
            repo_root.display()
        );
    }
    Ok(canonical)
}

fn validate_query(query: &str) -> anyhow::Result<()> {
    if query.is_empty() {
        bail!("query must not be empty");
    }
    if query.len() > SOURCE_SEARCH_MAX_QUERY_BYTES {
        bail!(
            "query is too large ({} bytes, max {})",
            query.len(),
            SOURCE_SEARCH_MAX_QUERY_BYTES
        );
    }
    if query.contains(['\r', '\n']) {
        bail!("query must be a single line");
    }
    Ok(())
}

pub fn should_descend_source_path(
    path: &Path,
    include_generated: bool,
    include_vendor: bool,
) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    let name = name.to_ascii_lowercase();
    if is_vendor_dir(&name) {
        return include_vendor;
    }
    if is_generated_dir(&name) {
        return include_generated;
    }
    !is_always_ignored_dir(&name)
}

pub fn should_scan_source_file(
    path: &Path,
    include_generated: bool,
    include_vendor: bool,
    include_locks: bool,
) -> bool {
    if !include_vendor && has_named_component(path, is_vendor_dir) {
        return false;
    }
    if !include_generated && has_named_component(path, is_generated_dir) {
        return false;
    }
    if is_lockfile(path) {
        return include_locks;
    }
    looks_like_source_path(path)
}

fn has_named_component(path: &Path, predicate: fn(&str) -> bool) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| predicate(&name.to_ascii_lowercase()))
    })
}

fn is_vendor_dir(name: &str) -> bool {
    matches!(name, "vendor" | "third_party" | "node_modules")
}

fn is_generated_dir(name: &str) -> bool {
    matches!(name, "generated" | "target" | "dist" | "build" | ".next")
}

fn is_always_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".venv"
            | "venv"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".ruff_cache"
            | ".turbo"
            | "coverage"
            | ".cache"
    )
}

fn is_lockfile(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            matches!(
                name.as_str(),
                "cargo.lock"
                    | "pnpm-lock.yaml"
                    | "package-lock.json"
                    | "packages.lock.json"
                    | "npm-shrinkwrap.json"
                    | "yarn.lock"
                    | "uv.lock"
                    | ".terraform.lock.hcl"
                    | "gradle.lockfile"
                    | "bun.lockb"
                    | "package.resolved"
                    | "go.sum"
                    | "go.work.sum"
            ) || name.ends_with(".lock")
        })
}

fn looks_like_source_path(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return true;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "rs" | "toml"
            | "md"
            | "json"
            | "jsonl"
            | "yaml"
            | "yml"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "py"
            | "sh"
            | "ps1"
            | "css"
            | "html"
            | "txt"
            | "go"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hh"
            | "hpp"
            | "hxx"
            | "inl"
            | "m"
            | "mm"
            | "cs"
            | "java"
            | "kt"
            | "kts"
            | "scala"
            | "sc"
            | "swift"
            | "sql"
            | "proto"
            | "graphql"
            | "gql"
            | "rb"
            | "php"
            | "dart"
            | "lua"
            | "r"
            | "jl"
            | "ex"
            | "exs"
            | "erl"
            | "hrl"
            | "fs"
            | "fsx"
            | "fsi"
            | "vb"
            | "zig"
            | "nim"
            | "hs"
            | "lhs"
            | "ml"
            | "mli"
            | "clj"
            | "cljs"
            | "cljc"
            | "edn"
            | "groovy"
            | "gradle"
            | "vue"
            | "svelte"
            | "astro"
            | "xml"
            | "xsd"
            | "xsl"
            | "hcl"
            | "tf"
            | "tfvars"
            | "nix"
            | "cmake"
            | "bzl"
            | "bazel"
            | "ini"
            | "cfg"
            | "conf"
            | "properties"
            | "thrift"
            | "capnp"
            | "asm"
            | "s"
            | "sol"
    )
}

fn bounded_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn relative_display(repo_root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(repo_root).unwrap_or(path);
    if relative.as_os_str().is_empty() {
        return ".".to_string();
    }
    relative.to_string_lossy().replace('\\', "/")
}

fn print_output<T: Serialize>(output: &T, json: bool, print_human: fn(&T)) -> anyhow::Result<()> {
    if json {
        let mut stdout = std::io::stdout();
        serde_json::to_writer_pretty(&mut stdout, output)?;
        writeln!(&mut stdout)?;
    } else {
        print_human(output);
    }
    Ok(())
}

fn print_search_human(output: &SourceSearchOutput) {
    for source_match in &output.matches {
        println!(
            "{}:{}-{}",
            source_match.path, source_match.start_line, source_match.end_line
        );
        for line in &source_match.lines {
            println!("{}: {}", line.line_number, line.text);
        }
    }
    if output.truncated {
        eprintln!("source search truncated: {:?}", output.truncated_reason);
    }
    if output.coverage.files_changed_during_read > 0 {
        eprintln!(
            "source search skipped {} file(s) that changed while being read",
            output.coverage.files_changed_during_read
        );
    }
}

fn print_span_human(output: &ReadFileSpanOutput) {
    for line in &output.lines {
        println!("{}: {}", line.line_number, line.text);
    }
    if output.truncated {
        eprintln!("source span truncated by configured limits");
    }
}

#[cfg(test)]
#[path = "source_search_tests.rs"]
mod tests;
