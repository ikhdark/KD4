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
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::bail;
use clap::ArgAction;
use clap::Parser;
use codex_file_system::open_confined_file;
use ignore::Match;
use ignore::WalkBuilder;
use ignore::gitignore::Gitignore;
use ignore::gitignore::GitignoreBuilder;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
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
const SOURCE_SEARCH_HYDRATION_MAX_BYTES: usize = 5 * 1024;
const SOURCE_SEARCH_HYDRATION_LINES: usize = 120;
const SOURCE_SEARCH_HYDRATION_MAX_SPANS: usize = 8;
const SOURCE_SEARCH_HYDRATION_PACKET_SCHEMA_VERSION: u32 = 1;

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
    pub hydrate_selected_span: bool,
    pub hydration_candidates: Vec<SourceSearchHydrationCandidate>,
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
            hydrate_selected_span: true,
            hydration_candidates: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSearchHydrationCandidate {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: SourceSearchHydrationCandidateKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceSearchHydrationCandidateKind {
    AuthoritativeDefinition,
    StructuredContext,
}

#[derive(Debug, Clone)]
pub struct ReadFileSpanOptions {
    pub repo_root: PathBuf,
    pub path: PathBuf,
    pub start_line: usize,
    pub line_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SourceSearchOutput {
    pub query: String,
    pub roots: Vec<String>,
    pub truncated: bool,
    pub truncated_reason: Option<SourceTruncatedReason>,
    pub coverage_complete: bool,
    pub coverage_note: Option<String>,
    pub coverage: SourceSearchCoverage,
    pub matches: Vec<SourceSearchMatch>,
    pub hydration_status: SourceSearchHydrationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hydrated_span: Option<SourceSearchHydratedSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydration_packet: Option<SourceSearchHydrationPacket>,
    #[serde(skip)]
    pub diagnostics: SourceSearchDiagnostics,
}

impl PartialEq for SourceSearchOutput {
    fn eq(&self, other: &Self) -> bool {
        self.query == other.query
            && self.roots == other.roots
            && self.truncated == other.truncated
            && self.truncated_reason == other.truncated_reason
            && self.coverage_complete == other.coverage_complete
            && self.coverage_note == other.coverage_note
            && self.coverage == other.coverage
            && self.matches == other.matches
            && self.hydration_status == other.hydration_status
            && self.hydrated_span == other.hydrated_span
            && self.hydration_packet == other.hydration_packet
    }
}

impl Eq for SourceSearchOutput {}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceSearchHydrationStatus {
    Disabled,
    HydratedAuthoritativeDefinition,
    HydratedStructuredContext,
    HydratedDeterministicWindow,
    HydratedBoundedPacket,
    PartiallyHydratedBoundedPacket,
    SkippedCoverageIncomplete,
    SkippedIndexIncomplete,
    SkippedNoUniqueMatch,
    SkippedObservationUnavailable,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SourceSearchHydratedSpan {
    pub content_hash: String,
    pub observation: ReadFileSpanOutput,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SourceSearchHydrationPacket {
    pub schema_version: u32,
    pub observation_set_id: String,
    pub exact_content_byte_limit: usize,
    pub exact_content_bytes: usize,
    pub spans: Vec<SourceSearchHydrationPacketSpan>,
    pub issues: Vec<SourceSearchHydrationIssue>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SourceSearchHydrationPacketSpan {
    pub id: String,
    pub match_ids: Vec<String>,
    pub path: String,
    pub requested_start_line: usize,
    pub requested_end_line: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub file_content_hash: String,
    pub span_content_hash: String,
    pub selection: SourceSearchHydrationSelection,
    pub truncated: bool,
    pub exact_content: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SourceSearchHydrationSelection {
    AuthoritativeDefinition,
    StructuredContext,
    DeterministicWindow,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SourceSearchHydrationIssue {
    pub match_id: String,
    pub reason: SourceSearchHydrationIssueReason,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceSearchHydrationIssueReason {
    AmbiguousAuthoritativeCandidate,
    AmbiguousStructuredCandidate,
    ByteCap,
    SpanCap,
    ObservationUnavailable,
    OversizedMatchedLine,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct SourceSearchCoverage {
    pub walked_entries: usize,
    pub ignored_entries: usize,
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
    #[serde(default)]
    pub index_complete: bool,
    #[serde(default)]
    pub context_complete: bool,
    #[serde(default)]
    pub indexed_matches: usize,
    #[serde(default)]
    pub omitted_contexts: usize,
    #[serde(default)]
    pub result_cap_reached: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceSearchDiagnostics {
    pub total_micros: u64,
    pub first_match_micros: Option<u64>,
    pub traversal_micros: u64,
    pub file_scan_match_micros: u64,
    pub projection_micros: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SourceSearchMatch {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub file_id: String,
    pub path: String,
    #[serde(default)]
    pub source_revision: String,
    pub source_map_route: Option<String>,
    pub line_number: usize,
    #[serde(default)]
    pub matched_content: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(default)]
    pub context_complete: bool,
    pub lines: Vec<SourceLine>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SourceLine {
    pub line_number: usize,
    pub text: String,
    pub text_truncated: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
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
    #[serde(default)]
    pub full_file_sha256: String,
    #[serde(default)]
    pub requested_content_sha256: String,
    #[serde(default)]
    pub requested_bytes: usize,
    #[serde(default)]
    pub exact_content: String,
    #[serde(default)]
    pub chunks: Vec<SourceReadChunk>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SourceReadChunk {
    pub id: String,
    pub start_line: usize,
    pub end_line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub exact_bytes: usize,
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

    let traversal_started = Instant::now();
    for root in &roots {
        if accumulator.should_stop() {
            break;
        }
        scan_root(&repo_root, root, &mut accumulator, walk_limits)?;
    }
    accumulator.record_traversal_duration(traversal_started.elapsed());

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
    let line_ranges = exact_line_ranges(&text);
    let requested_byte_start = line_ranges
        .get(start_index)
        .map_or(text.len(), |(start, _)| *start);
    let requested_byte_end = end_index
        .checked_sub(1)
        .and_then(|index| line_ranges.get(index))
        .map_or(requested_byte_start, |(_, end)| *end);
    let exact_content = text[requested_byte_start..requested_byte_end].to_string();
    let full_file_sha256 = format!("{:x}", Sha256::digest(text.as_bytes()));
    let requested_content_sha256 = format!("{:x}", Sha256::digest(exact_content.as_bytes()));
    let chunks = source_read_chunks(
        &line_ranges[start_index..end_index],
        start_index,
        requested_byte_start,
        &requested_content_sha256,
    );

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
        full_file_sha256,
        requested_content_sha256,
        requested_bytes: exact_content.len(),
        exact_content,
        chunks,
    })
}

/// Retains only the requested absolute line intervals in an exact read result.
///
/// Coverage consumers use this after discovering that some of a read is
/// already present in the current turn. Rebuilding the exact payload and its
/// chunks here keeps the rendered lines, canonical artifact, and projection
/// ranges on the same evidence boundary.
pub fn retain_read_file_span_intervals(
    output: &mut ReadFileSpanOutput,
    intervals: &[(usize, usize)],
) {
    let Some(first_source_line) = output.chunks.first().map(|chunk| chunk.start_line) else {
        output.lines.clear();
        output.start_line = None;
        output.end_line = None;
        output.bytes_returned = 0;
        output.requested_bytes = 0;
        output.exact_content.clear();
        output.requested_content_sha256 = format!("{:x}", Sha256::digest([]));
        output.chunks.clear();
        return;
    };
    let source_ranges = exact_line_ranges(&output.exact_content);
    let mut retained = intervals
        .iter()
        .filter_map(|(start_line, end_line)| {
            let start_line = (*start_line).max(first_source_line);
            let last_source_line =
                first_source_line.saturating_add(source_ranges.len().saturating_sub(1));
            let end_line = (*end_line).min(last_source_line);
            (start_line <= end_line).then_some((start_line, end_line))
        })
        .collect::<Vec<_>>();
    retained.sort_unstable();
    let mut merged = Vec::<(usize, usize)>::with_capacity(retained.len());
    for (start_line, end_line) in retained {
        if let Some((_, prior_end)) = merged.last_mut()
            && start_line <= prior_end.saturating_add(1)
        {
            *prior_end = (*prior_end).max(end_line);
        } else {
            merged.push((start_line, end_line));
        }
    }

    let mut exact_content = String::new();
    let mut retained_segments = Vec::new();
    for (start_line, end_line) in &merged {
        let start_index = start_line.saturating_sub(first_source_line);
        let end_index = end_line.saturating_sub(first_source_line).saturating_add(1);
        let Some((byte_start, _)) = source_ranges.get(start_index).copied() else {
            continue;
        };
        let Some((_, byte_end)) = end_index
            .checked_sub(1)
            .and_then(|index| source_ranges.get(index))
            .copied()
        else {
            continue;
        };
        let artifact_start = exact_content.len();
        exact_content.push_str(&output.exact_content[byte_start..byte_end]);
        retained_segments.push((*start_line, *end_line, artifact_start, exact_content.len()));
    }

    output.lines.retain(|line| {
        merged.iter().any(|(start_line, end_line)| {
            line.line_number >= *start_line && line.line_number <= *end_line
        })
    });
    output.start_line = output.lines.first().map(|line| line.line_number);
    output.end_line = output.lines.last().map(|line| line.line_number);
    output.bytes_returned = output.lines.iter().map(|line| line.text.len()).sum();
    output.requested_bytes = exact_content.len();
    output.requested_content_sha256 = format!("{:x}", Sha256::digest(exact_content.as_bytes()));
    output.chunks = retained_segments
        .into_iter()
        .flat_map(|(start_line, _end_line, byte_start, byte_end)| {
            let segment_ranges = exact_line_ranges(&exact_content[byte_start..byte_end])
                .into_iter()
                .map(|(start, end)| (start + byte_start, end + byte_start))
                .collect::<Vec<_>>();
            source_read_chunks(
                &segment_ranges,
                start_line.saturating_sub(1),
                0,
                &output.requested_content_sha256,
            )
        })
        .collect();
    output.exact_content = exact_content;
}

fn exact_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut cursor = 0_usize;
    text.split_inclusive('\n')
        .map(|line| {
            let start = cursor;
            cursor = cursor.saturating_add(line.len());
            (start, cursor)
        })
        .collect()
}

fn source_read_chunks(
    line_ranges: &[(usize, usize)],
    zero_based_start_line: usize,
    requested_byte_start: usize,
    content_hash: &str,
) -> Vec<SourceReadChunk> {
    const MAX_CHUNK_LINES: usize = 40;
    const MAX_CHUNK_BYTES: usize = 8 * 1024;
    let mut chunks = Vec::new();
    let mut cursor = 0_usize;
    while cursor < line_ranges.len() {
        let chunk_start = cursor;
        let absolute_start = line_ranges[cursor].0;
        let mut chunk_end = cursor + 1;
        while chunk_end < line_ranges.len() && chunk_end - chunk_start < MAX_CHUNK_LINES {
            let candidate_bytes = line_ranges[chunk_end].1.saturating_sub(absolute_start);
            if candidate_bytes > MAX_CHUNK_BYTES {
                break;
            }
            chunk_end += 1;
        }
        let absolute_end = line_ranges[chunk_end - 1].1;
        let start_line = zero_based_start_line + chunk_start + 1;
        let end_line = zero_based_start_line + chunk_end;
        let byte_start = absolute_start.saturating_sub(requested_byte_start);
        let byte_end = absolute_end.saturating_sub(requested_byte_start);
        chunks.push(SourceReadChunk {
            id: format!(
                "src:{}:L{start_line}-L{end_line}",
                &content_hash[..content_hash.len().min(16)]
            ),
            start_line,
            end_line,
            byte_start,
            byte_end,
            exact_bytes: byte_end.saturating_sub(byte_start),
        });
        cursor = chunk_end;
    }
    chunks
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
    unscoped: bool,
    hydrate_selected_span: bool,
    hydration_candidates: Vec<SourceSearchHydrationCandidate>,
    hydration_observations: HashMap<String, CapturedHydrationObservation>,
    unique_observation: Option<UniqueSearchObservation>,
    started_at: Instant,
    traversal_duration: Duration,
    file_scan_match_duration: Duration,
    first_match_duration: Option<Duration>,
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
            unscoped: options.roots.is_empty(),
            hydrate_selected_span: options.hydrate_selected_span,
            hydration_candidates: options.hydration_candidates.clone(),
            hydration_observations: HashMap::new(),
            unique_observation: None,
            started_at: Instant::now(),
            traversal_duration: Duration::ZERO,
            file_scan_match_duration: Duration::ZERO,
            first_match_duration: None,
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
        self.consider_walked_file(file_len)
    }

    /// Records a file already accepted by the walker entry filter.
    pub fn consider_walked_file(&mut self, file_len: usize) -> bool {
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
        let content_hash = format!("{:x}", Sha256::digest(&bytes));
        let Ok(text) = String::from_utf8(bytes) else {
            self.state.files_skipped_non_utf8 = self.state.files_skipped_non_utf8.saturating_add(1);
            return;
        };
        let matches_before = self.state.total_matches;
        let returned_matches_before = self.state.matches.len();
        collect_matches(
            relative_path,
            &text,
            MatchParameters {
                case_sensitive: self.case_sensitive,
                context_lines: self.context_lines,
                query: &self.query,
                query_cmp: &self.query_cmp,
                source_revision: &content_hash,
            },
            &mut self.state,
        );
        for matched in self.state.matches[returned_matches_before..]
            .iter()
            .cloned()
        {
            let observation = capture_hydration_observation(
                &matched,
                &text,
                &content_hash,
                &self.hydration_candidates,
            );
            self.hydration_observations
                .insert(matched.id.clone(), observation);
        }
        if self.state.total_matches == 1 && self.state.total_matches > matches_before {
            self.unique_observation = Some(UniqueSearchObservation {
                path: relative_path.to_path_buf(),
                bytes: text.into_bytes(),
                content_hash,
            });
        } else if self.state.total_matches != 1 {
            self.unique_observation = None;
        }
        if self.first_match_duration.is_none() && self.state.total_matches > matches_before {
            self.first_match_duration = Some(self.started_at.elapsed());
        }
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

    pub fn record_ignored_entries(&mut self, count: usize) {
        self.state.ignored_entries = self.state.ignored_entries.saturating_add(count);
    }

    pub fn record_traversal_duration(&mut self, duration: Duration) {
        self.traversal_duration = self.traversal_duration.saturating_add(duration);
    }

    pub fn record_file_scan_match_duration(&mut self, duration: Duration) {
        self.file_scan_match_duration = self.file_scan_match_duration.saturating_add(duration);
    }

    pub fn mark_file_changed_during_read(&mut self) {
        self.state.files_changed_during_read =
            self.state.files_changed_during_read.saturating_add(1);
    }

    pub fn mark_filesystem_error(&mut self) {
        self.state.filesystem_errors = self.state.filesystem_errors.saturating_add(1);
    }

    pub fn finish(mut self, roots: Vec<String>) -> SourceSearchOutput {
        let projection_started = Instant::now();
        self.state.matches.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.line_number.cmp(&right.line_number))
                .then_with(|| left.id.cmp(&right.id))
        });
        let coverage_limit = self.state.coverage_limit;
        let mut result_limit = self.state.result_limit;
        let coverage = SourceSearchCoverage {
            walked_entries: self.state.walk_entries_seen,
            ignored_entries: self.state.ignored_entries,
            files_scanned: self.state.files_scanned,
            files_skipped_too_large: self.state.files_skipped_too_large,
            files_skipped_non_utf8: self.state.files_skipped_non_utf8,
            files_changed_during_read: self.state.files_changed_during_read,
            filesystem_errors: self.state.filesystem_errors,
            bytes_scanned: self.state.bytes_scanned,
            result_bytes: 0,
            total_matches: self.state.total_matches,
            matches_returned: self.state.matches.len(),
            max_matches: self.state.max_matches,
            max_files: SOURCE_SEARCH_MAX_FILES,
            max_bytes: SOURCE_SEARCH_MAX_BYTES,
            max_file_bytes: SOURCE_SEARCH_MAX_FILE_BYTES,
            max_result_bytes: SOURCE_SEARCH_MAX_RESULT_BYTES,
            index_complete: coverage_limit.is_none() && result_limit.is_none(),
            context_complete: true,
            indexed_matches: self.state.matches.len(),
            omitted_contexts: 0,
            result_cap_reached: result_limit == Some(SourceTruncatedReason::MaxMatches),
        };
        let mut output = SourceSearchOutput {
            query: self.query,
            roots,
            truncated: false,
            truncated_reason: None,
            coverage_complete: true,
            coverage_note: None,
            coverage,
            matches: self.state.matches,
            hydration_status: SourceSearchHydrationStatus::SkippedNoUniqueMatch,
            hydrated_span: None,
            hydration_packet: None,
            diagnostics: SourceSearchDiagnostics::default(),
        };
        update_search_output_status(&mut output, coverage_limit, result_limit, self.unscoped);
        apply_search_hydration(
            &mut output,
            self.hydrate_selected_span,
            self.unique_observation.take(),
            &self.hydration_observations,
            &self.hydration_candidates,
        );
        let result_cap_reached = result_limit == Some(SourceTruncatedReason::MaxMatches);
        let mut identity_cap_reached = false;
        loop {
            if update_serialized_result_bytes(&mut output) <= SOURCE_SEARCH_MAX_RESULT_BYTES {
                break;
            }
            if let Some(source_match) = output
                .matches
                .iter_mut()
                .rev()
                .find(|source_match| !source_match.lines.is_empty())
            {
                source_match.lines.clear();
                source_match.context_complete = false;
                output.coverage.context_complete = false;
                output.coverage.omitted_contexts =
                    output.coverage.omitted_contexts.saturating_add(1);
            } else if let Some(source_match) = output
                .matches
                .iter_mut()
                .rev()
                .find(|source_match| !source_match.matched_content.is_empty())
            {
                source_match.matched_content.clear();
            } else if output.hydrated_span.take().is_some() {
                output.hydration_status =
                    SourceSearchHydrationStatus::SkippedObservationUnavailable;
            } else if output.matches.pop().is_some() {
                identity_cap_reached = true;
            } else {
                break;
            }
            result_limit.get_or_insert(SourceTruncatedReason::MaxResultBytes);
            update_search_output_status(&mut output, coverage_limit, result_limit, self.unscoped);
        }
        if identity_cap_reached && output.hydration_packet.take().is_some() {
            output.hydration_status = SourceSearchHydrationStatus::SkippedIndexIncomplete;
        }
        output.coverage.result_cap_reached = result_cap_reached || identity_cap_reached;
        output.coverage.index_complete =
            output.coverage_complete && !result_cap_reached && !identity_cap_reached;
        output.coverage.matches_returned = output.matches.len();
        output.coverage.indexed_matches = output.matches.len();
        update_serialized_result_bytes(&mut output);
        let projection_duration = projection_started.elapsed();
        output.diagnostics = SourceSearchDiagnostics {
            total_micros: duration_micros(self.started_at.elapsed()),
            first_match_micros: self.first_match_duration.map(duration_micros),
            traversal_micros: duration_micros(self.traversal_duration),
            file_scan_match_micros: duration_micros(self.file_scan_match_duration),
            projection_micros: duration_micros(projection_duration),
        };
        output
    }
}

fn update_serialized_result_bytes(output: &mut SourceSearchOutput) -> usize {
    for _ in 0..4 {
        let serialized_bytes = serde_json::to_vec_pretty(output)
            .map(|bytes| bytes.len().saturating_add(1))
            .unwrap_or(usize::MAX);
        if output.coverage.result_bytes == serialized_bytes {
            break;
        }
        output.coverage.result_bytes = serialized_bytes;
    }
    output.coverage.result_bytes
}

struct UniqueSearchObservation {
    path: PathBuf,
    bytes: Vec<u8>,
    content_hash: String,
}

#[derive(Clone)]
enum CapturedHydrationObservation {
    Span(CapturedHydrationSpan),
    Issue(SourceSearchHydrationIssueReason),
}

#[derive(Clone)]
struct CapturedHydrationSpan {
    match_ids: Vec<String>,
    match_lines: Vec<usize>,
    path: String,
    requested_start_line: usize,
    requested_end_line: usize,
    start_line: usize,
    end_line: usize,
    file_content_hash: String,
    selection: SourceSearchHydrationSelection,
    truncated: bool,
    exact_content: String,
}

fn apply_search_hydration(
    output: &mut SourceSearchOutput,
    enabled: bool,
    unique_observation: Option<UniqueSearchObservation>,
    observations: &HashMap<String, CapturedHydrationObservation>,
    candidates: &[SourceSearchHydrationCandidate],
) {
    if !enabled {
        output.hydration_status = SourceSearchHydrationStatus::Disabled;
        return;
    }
    if !output.coverage_complete {
        output.hydration_status = SourceSearchHydrationStatus::SkippedCoverageIncomplete;
        return;
    }
    match output.coverage.total_matches {
        0 => {
            output.hydration_status = SourceSearchHydrationStatus::SkippedNoUniqueMatch;
        }
        1 => apply_unique_search_hydration(
            output,
            /* enabled */ true,
            unique_observation,
            candidates,
        ),
        _ if !output.coverage.index_complete => {
            output.hydration_status = SourceSearchHydrationStatus::SkippedIndexIncomplete;
        }
        _ => apply_bounded_search_hydration_packet(output, observations),
    }
}

fn capture_hydration_observation(
    matched: &SourceSearchMatch,
    text: &str,
    content_hash: &str,
    candidates: &[SourceSearchHydrationCandidate],
) -> CapturedHydrationObservation {
    let selected = select_hydration_candidate(matched, candidates);
    let (requested_start_line, requested_end_line, selection) = match selected {
        Ok(Some(candidate)) => (
            candidate.start_line,
            candidate.end_line,
            match candidate.kind {
                SourceSearchHydrationCandidateKind::AuthoritativeDefinition => {
                    SourceSearchHydrationSelection::AuthoritativeDefinition
                }
                SourceSearchHydrationCandidateKind::StructuredContext => {
                    SourceSearchHydrationSelection::StructuredContext
                }
            },
        ),
        Ok(None) => {
            let start_line = matched.line_number.saturating_sub(20).max(1);
            (
                start_line,
                start_line.saturating_add(SOURCE_SEARCH_HYDRATION_LINES.saturating_sub(1)),
                SourceSearchHydrationSelection::DeterministicWindow,
            )
        }
        Err(reason) => return CapturedHydrationObservation::Issue(reason),
    };
    capture_exact_hydration_span(
        matched,
        text,
        content_hash,
        requested_start_line,
        requested_end_line,
        selection,
    )
    .map_or_else(
        CapturedHydrationObservation::Issue,
        CapturedHydrationObservation::Span,
    )
}

fn select_hydration_candidate<'a>(
    matched: &SourceSearchMatch,
    candidates: &'a [SourceSearchHydrationCandidate],
) -> Result<Option<&'a SourceSearchHydrationCandidate>, SourceSearchHydrationIssueReason> {
    for (kind, ambiguity) in [
        (
            SourceSearchHydrationCandidateKind::AuthoritativeDefinition,
            SourceSearchHydrationIssueReason::AmbiguousAuthoritativeCandidate,
        ),
        (
            SourceSearchHydrationCandidateKind::StructuredContext,
            SourceSearchHydrationIssueReason::AmbiguousStructuredCandidate,
        ),
    ] {
        let mut covering = candidates
            .iter()
            .filter(|candidate| {
                candidate.kind == kind
                    && candidate.path.replace('\\', "/") == matched.path
                    && candidate.start_line > 0
                    && candidate.end_line >= candidate.start_line
                    && candidate.start_line <= matched.line_number
                    && candidate.end_line >= matched.line_number
            })
            .collect::<Vec<_>>();
        covering.sort_by_key(|candidate| (candidate.start_line, candidate.end_line));
        covering.dedup_by_key(|candidate| (candidate.start_line, candidate.end_line));
        match covering.as_slice() {
            [] => {}
            [candidate] => return Ok(Some(*candidate)),
            _ => return Err(ambiguity),
        }
    }
    Ok(None)
}

fn capture_exact_hydration_span(
    matched: &SourceSearchMatch,
    text: &str,
    content_hash: &str,
    requested_start_line: usize,
    requested_end_line: usize,
    selection: SourceSearchHydrationSelection,
) -> Result<CapturedHydrationSpan, SourceSearchHydrationIssueReason> {
    let line_ranges = exact_line_ranges(text);
    let matched_index = matched.line_number.saturating_sub(1);
    let requested_start_index = requested_start_line.saturating_sub(1);
    let requested_end_index = requested_end_line.min(line_ranges.len());
    if matched_index >= line_ranges.len()
        || matched_index < requested_start_index
        || matched_index >= requested_end_index
    {
        return Err(SourceSearchHydrationIssueReason::ObservationUnavailable);
    }
    let matched_range = line_ranges[matched_index];
    if matched_range.1.saturating_sub(matched_range.0) > SOURCE_SEARCH_HYDRATION_MAX_BYTES {
        return Err(SourceSearchHydrationIssueReason::OversizedMatchedLine);
    }

    let mut start_index = matched_index;
    let mut end_index = matched_index + 1;
    let mut left_open = start_index > requested_start_index;
    let mut right_open = end_index < requested_end_index;
    while left_open || right_open {
        let mut expanded = false;
        if left_open {
            let candidate_start = start_index - 1;
            let bytes = line_ranges[end_index - 1]
                .1
                .saturating_sub(line_ranges[candidate_start].0);
            if bytes <= SOURCE_SEARCH_HYDRATION_MAX_BYTES {
                start_index = candidate_start;
                expanded = true;
                left_open = start_index > requested_start_index;
            } else {
                left_open = false;
            }
        }
        if right_open {
            let candidate_end = end_index + 1;
            let bytes = line_ranges[candidate_end - 1]
                .1
                .saturating_sub(line_ranges[start_index].0);
            if bytes <= SOURCE_SEARCH_HYDRATION_MAX_BYTES {
                end_index = candidate_end;
                expanded = true;
                right_open = end_index < requested_end_index;
            } else {
                right_open = false;
            }
        }
        if !expanded {
            break;
        }
    }

    let byte_start = line_ranges[start_index].0;
    let byte_end = line_ranges[end_index - 1].1;
    Ok(CapturedHydrationSpan {
        match_ids: vec![matched.id.clone()],
        match_lines: vec![matched.line_number],
        path: matched.path.clone(),
        requested_start_line,
        requested_end_line,
        start_line: start_index + 1,
        end_line: end_index,
        file_content_hash: content_hash.to_string(),
        selection,
        truncated: start_index != requested_start_index
            || end_index != requested_end_index
            || requested_end_line > line_ranges.len(),
        exact_content: text[byte_start..byte_end].to_string(),
    })
}

fn apply_bounded_search_hydration_packet(
    output: &mut SourceSearchOutput,
    observations: &HashMap<String, CapturedHydrationObservation>,
) {
    let mut grouped = Vec::<CapturedHydrationSpan>::new();
    let mut issues = Vec::new();
    for matched in &output.matches {
        match observations.get(&matched.id) {
            Some(CapturedHydrationObservation::Span(captured)) => {
                if let Some(existing) = grouped.iter_mut().find(|existing| {
                    existing.path == captured.path
                        && existing.file_content_hash == captured.file_content_hash
                        && existing.start_line == captured.start_line
                        && existing.end_line == captured.end_line
                        && existing.exact_content == captured.exact_content
                }) {
                    existing
                        .match_ids
                        .extend(captured.match_ids.iter().cloned());
                    existing
                        .match_lines
                        .extend(captured.match_lines.iter().copied());
                    existing.match_ids.sort();
                    existing.match_ids.dedup();
                    existing.match_lines.sort_unstable();
                    existing.match_lines.dedup();
                    existing.selection = existing.selection.min(captured.selection);
                    existing.requested_start_line = existing
                        .requested_start_line
                        .min(captured.requested_start_line);
                    existing.requested_end_line =
                        existing.requested_end_line.max(captured.requested_end_line);
                    existing.truncated |= captured.truncated;
                } else {
                    grouped.push(captured.clone());
                }
            }
            Some(CapturedHydrationObservation::Issue(reason)) => {
                issues.push(SourceSearchHydrationIssue {
                    match_id: matched.id.clone(),
                    reason: *reason,
                });
            }
            None => issues.push(SourceSearchHydrationIssue {
                match_id: matched.id.clone(),
                reason: SourceSearchHydrationIssueReason::ObservationUnavailable,
            }),
        }
    }
    grouped.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| {
                left.match_lines
                    .iter()
                    .min()
                    .cmp(&right.match_lines.iter().min())
            })
            .then_with(|| {
                left.match_ids
                    .iter()
                    .min()
                    .cmp(&right.match_ids.iter().min())
            })
    });

    let mut spans = Vec::new();
    let mut exact_content_bytes = 0usize;
    for captured in grouped {
        if spans.len() >= SOURCE_SEARCH_HYDRATION_MAX_SPANS {
            push_hydration_issues(
                &mut issues,
                &captured.match_ids,
                SourceSearchHydrationIssueReason::SpanCap,
            );
            continue;
        }
        let remaining = SOURCE_SEARCH_HYDRATION_MAX_BYTES.saturating_sub(exact_content_bytes);
        let captured_match_ids = captured.match_ids.clone();
        let Some(captured) = fit_captured_hydration_span(captured, remaining) else {
            push_hydration_issues(
                &mut issues,
                &captured_match_ids,
                SourceSearchHydrationIssueReason::ByteCap,
            );
            continue;
        };
        exact_content_bytes = exact_content_bytes.saturating_add(captured.exact_content.len());
        spans.push(finalize_hydration_packet_span(captured));
    }

    issues.sort_by_key(|issue| {
        output
            .matches
            .iter()
            .position(|matched| matched.id == issue.match_id)
            .unwrap_or(usize::MAX)
    });
    let observation_set_id = hydration_observation_set_id(output, &spans, &issues);
    output.hydration_status = if issues.is_empty() {
        SourceSearchHydrationStatus::HydratedBoundedPacket
    } else {
        SourceSearchHydrationStatus::PartiallyHydratedBoundedPacket
    };
    output.hydration_packet = Some(SourceSearchHydrationPacket {
        schema_version: SOURCE_SEARCH_HYDRATION_PACKET_SCHEMA_VERSION,
        observation_set_id,
        exact_content_byte_limit: SOURCE_SEARCH_HYDRATION_MAX_BYTES,
        exact_content_bytes,
        spans,
        issues,
    });
}

fn push_hydration_issues(
    issues: &mut Vec<SourceSearchHydrationIssue>,
    match_ids: &[String],
    reason: SourceSearchHydrationIssueReason,
) {
    issues.extend(
        match_ids
            .iter()
            .cloned()
            .map(|match_id| SourceSearchHydrationIssue { match_id, reason }),
    );
}

fn fit_captured_hydration_span(
    mut captured: CapturedHydrationSpan,
    byte_limit: usize,
) -> Option<CapturedHydrationSpan> {
    if captured.exact_content.len() <= byte_limit {
        return Some(captured);
    }
    let line_ranges = exact_line_ranges(&captured.exact_content);
    let first_match_line = *captured.match_lines.iter().min()?;
    let last_match_line = *captured.match_lines.iter().max()?;
    let required_start = first_match_line.checked_sub(captured.start_line)?;
    let required_end = last_match_line
        .checked_sub(captured.start_line)?
        .checked_add(1)?;
    if required_start >= line_ranges.len() || required_end > line_ranges.len() {
        return None;
    }
    let required_bytes = line_ranges[required_end - 1]
        .1
        .saturating_sub(line_ranges[required_start].0);
    if required_bytes > byte_limit {
        return None;
    }

    let mut start = required_start;
    let mut end = required_end;
    let mut left_open = start > 0;
    let mut right_open = end < line_ranges.len();
    while left_open || right_open {
        let mut expanded = false;
        if left_open {
            let candidate_start = start - 1;
            let bytes = line_ranges[end - 1]
                .1
                .saturating_sub(line_ranges[candidate_start].0);
            if bytes <= byte_limit {
                start = candidate_start;
                expanded = true;
                left_open = start > 0;
            } else {
                left_open = false;
            }
        }
        if right_open {
            let candidate_end = end + 1;
            let bytes = line_ranges[candidate_end - 1]
                .1
                .saturating_sub(line_ranges[start].0);
            if bytes <= byte_limit {
                end = candidate_end;
                expanded = true;
                right_open = end < line_ranges.len();
            } else {
                right_open = false;
            }
        }
        if !expanded {
            break;
        }
    }
    let original_start_line = captured.start_line;
    let byte_start = line_ranges[start].0;
    let byte_end = line_ranges[end - 1].1;
    captured.start_line = original_start_line + start;
    captured.end_line = original_start_line + end - 1;
    captured.exact_content = captured.exact_content[byte_start..byte_end].to_string();
    captured.truncated = true;
    Some(captured)
}

fn finalize_hydration_packet_span(
    captured: CapturedHydrationSpan,
) -> SourceSearchHydrationPacketSpan {
    let span_content_hash = format!("{:x}", Sha256::digest(captured.exact_content.as_bytes()));
    let id_hash = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}\0{}\0{}\0{}\0{}",
                captured.path,
                captured.file_content_hash,
                captured.start_line,
                captured.end_line,
                span_content_hash,
            )
            .as_bytes(),
        )
    );
    SourceSearchHydrationPacketSpan {
        id: format!("source-hydration:{}", &id_hash[..16]),
        match_ids: captured.match_ids,
        path: captured.path,
        requested_start_line: captured.requested_start_line,
        requested_end_line: captured.requested_end_line,
        start_line: captured.start_line,
        end_line: captured.end_line,
        file_content_hash: captured.file_content_hash,
        span_content_hash,
        selection: captured.selection,
        truncated: captured.truncated,
        exact_content: captured.exact_content,
    }
}

fn hydration_observation_set_id(
    output: &SourceSearchOutput,
    spans: &[SourceSearchHydrationPacketSpan],
    issues: &[SourceSearchHydrationIssue],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"source_search_hydration_packet_v1");
    hasher.update(output.query.as_bytes());
    for root in &output.roots {
        hasher.update([0]);
        hasher.update(root.as_bytes());
    }
    for matched in &output.matches {
        hasher.update([1]);
        hasher.update(matched.id.as_bytes());
        hasher.update(matched.path.as_bytes());
        hasher.update(matched.source_revision.as_bytes());
        hasher.update(matched.line_number.to_le_bytes());
    }
    hasher.update([2]);
    hasher.update(serde_json::to_vec(spans).unwrap_or_default());
    hasher.update([3]);
    hasher.update(serde_json::to_vec(issues).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

fn apply_unique_search_hydration(
    output: &mut SourceSearchOutput,
    enabled: bool,
    observation: Option<UniqueSearchObservation>,
    candidates: &[SourceSearchHydrationCandidate],
) {
    if !enabled {
        output.hydration_status = SourceSearchHydrationStatus::Disabled;
        return;
    }
    if !output.coverage_complete {
        output.hydration_status = SourceSearchHydrationStatus::SkippedCoverageIncomplete;
        return;
    }
    if output.coverage.total_matches != 1 || output.matches.len() != 1 {
        output.hydration_status = SourceSearchHydrationStatus::SkippedNoUniqueMatch;
        return;
    }
    let Some(observation) = observation else {
        output.hydration_status = SourceSearchHydrationStatus::SkippedObservationUnavailable;
        return;
    };
    let matched = &output.matches[0];
    let selected_candidate = [
        SourceSearchHydrationCandidateKind::AuthoritativeDefinition,
        SourceSearchHydrationCandidateKind::StructuredContext,
    ]
    .into_iter()
    .find_map(|kind| {
        let matches = candidates
            .iter()
            .filter(|candidate| {
                candidate.kind == kind
                    && candidate.path.replace('\\', "/") == matched.path
                    && candidate.start_line <= matched.line_number
                    && candidate.end_line >= matched.line_number
                    && candidate.start_line > 0
                    && candidate.end_line >= candidate.start_line
            })
            .collect::<Vec<_>>();
        (matches.len() == 1).then(|| (kind, matches[0]))
    });
    let start_line = selected_candidate.map_or_else(
        || matched.line_number.saturating_sub(20).max(1),
        |(_, candidate)| candidate.start_line,
    );
    let line_count = selected_candidate.map_or(SOURCE_SEARCH_HYDRATION_LINES, |(_, candidate)| {
        candidate
            .end_line
            .saturating_sub(candidate.start_line)
            .saturating_add(1)
            .min(SOURCE_SEARCH_HYDRATION_LINES)
    });
    let Ok(mut span) = read_file_span_from_bytes(
        observation.path.to_string_lossy().replace('\\', "/"),
        observation.bytes,
        start_line,
        line_count,
    ) else {
        output.hydration_status = SourceSearchHydrationStatus::SkippedObservationUnavailable;
        return;
    };
    let mut remaining = SOURCE_SEARCH_HYDRATION_MAX_BYTES;
    let mut bounded_lines = Vec::new();
    let original_line_count = span.lines.len();
    for mut line in std::mem::take(&mut span.lines) {
        if remaining == 0 {
            break;
        }
        let (text, text_truncated) = bounded_text(&line.text, remaining);
        remaining = remaining.saturating_sub(text.len());
        line.text = text;
        line.text_truncated |= text_truncated;
        bounded_lines.push(line);
        if text_truncated {
            break;
        }
    }
    let omitted = bounded_lines.len() < original_line_count;
    span.lines = bounded_lines;
    span.start_line = span.lines.first().map(|line| line.line_number);
    span.end_line = span.lines.last().map(|line| line.line_number);
    span.bytes_returned = SOURCE_SEARCH_HYDRATION_MAX_BYTES.saturating_sub(remaining);
    span.truncated |= omitted;
    output.hydration_status = match selected_candidate.map(|(kind, _)| kind) {
        Some(SourceSearchHydrationCandidateKind::AuthoritativeDefinition) => {
            SourceSearchHydrationStatus::HydratedAuthoritativeDefinition
        }
        Some(SourceSearchHydrationCandidateKind::StructuredContext) => {
            SourceSearchHydrationStatus::HydratedStructuredContext
        }
        None => SourceSearchHydrationStatus::HydratedDeterministicWindow,
    };
    output.hydrated_span = Some(SourceSearchHydratedSpan {
        content_hash: observation.content_hash,
        observation: span,
    });
}

struct SearchState {
    walk_directories_seen: usize,
    walk_entries_seen: usize,
    ignored_entries: usize,
    files_scanned: usize,
    files_skipped_too_large: usize,
    files_skipped_non_utf8: usize,
    files_changed_during_read: usize,
    filesystem_errors: usize,
    bytes_scanned: usize,
    total_matches: usize,
    max_matches: usize,
    coverage_limit: Option<SourceTruncatedReason>,
    result_limit: Option<SourceTruncatedReason>,
    matches: Vec<SourceSearchMatch>,
}

impl SearchState {
    fn new(max_matches: usize) -> Self {
        Self {
            walk_directories_seen: 0,
            walk_entries_seen: 0,
            ignored_entries: 0,
            files_scanned: 0,
            files_skipped_too_large: 0,
            files_skipped_non_utf8: 0,
            files_changed_during_read: 0,
            filesystem_errors: 0,
            bytes_scanned: 0,
            total_matches: 0,
            max_matches,
            coverage_limit: None,
            result_limit: None,
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

fn update_search_output_status(
    output: &mut SourceSearchOutput,
    coverage_limit: Option<SourceTruncatedReason>,
    result_limit: Option<SourceTruncatedReason>,
    unscoped: bool,
) {
    output.truncated_reason = source_truncated_reason(
        coverage_limit,
        result_limit,
        output.coverage.files_changed_during_read,
        output.coverage.filesystem_errors,
        output.coverage.files_skipped_too_large,
        output.coverage.files_skipped_non_utf8,
    );
    output.truncated = output.truncated_reason.is_some();
    output.coverage_complete = coverage_limit.is_none()
        && output.coverage.files_changed_during_read == 0
        && output.coverage.filesystem_errors == 0
        && output.coverage.files_skipped_too_large == 0
        && output.coverage.files_skipped_non_utf8 == 0;
    output.coverage.matches_returned = output.matches.len();
    output.coverage.index_complete =
        output.coverage_complete && result_limit != Some(SourceTruncatedReason::MaxMatches);
    output.coverage.result_cap_reached = result_limit == Some(SourceTruncatedReason::MaxMatches);
    if result_limit == Some(SourceTruncatedReason::MaxResultBytes) {
        output.coverage.context_complete = false;
    }
    let cap_reason = coverage_limit.filter(|reason| reason.is_search_cap());
    output.coverage_note = unscoped.then_some(cap_reason).flatten().map(|cap_reason| {
        let result_summary = if output.coverage.total_matches == 0 {
            "No matches were found in the scanned portion of the repository."
        } else {
            "Returned matches cover only the scanned portion of the repository."
        };
        format!(
            "{result_summary} Coverage is incomplete because this unscoped search reached the {}. Narrow `paths` or use `locate_task` to identify the owning scope.",
            cap_reason.display_name()
        )
    });
}

impl SourceTruncatedReason {
    fn is_search_cap(self) -> bool {
        matches!(
            self,
            Self::MaxMatches
                | Self::MaxFiles
                | Self::MaxBytes
                | Self::MaxResultBytes
                | Self::WalkLimit
        )
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::MaxMatches => "match-result cap",
            Self::MaxFiles => "file-scan cap",
            Self::MaxBytes => "scan-byte cap",
            Self::MaxResultBytes => "result-byte cap",
            Self::WalkLimit => "repository traversal cap",
            Self::FilesChangedDuringRead
            | Self::OversizedFiles
            | Self::NonUtf8Files
            | Self::FilesystemErrors => "coverage limit",
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
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
        recover_scan_result(scan_file(repo_root, root, accumulator, false), accumulator);
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
    let include_locks = accumulator.include_locks;
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
    let limit_entry_should_process = Arc::new(AtomicBool::new(true));
    let filter_limit_entry_should_process = Arc::clone(&limit_entry_should_process);
    let ignored_entries = Arc::new(AtomicUsize::new(0));
    let filter_ignored_entries = Arc::clone(&ignored_entries);
    let ignore_matcher = Arc::new(SourceIgnoreMatcher::new(root));
    let filter_ignore_matcher = Arc::clone(&ignore_matcher);
    let filter_root = repo_root.to_path_buf();
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
            let (should_yield, depth_exceeded) = source_walk_entry_filter(
                entry,
                &filter_root,
                walk_limits,
                include_generated,
                include_vendor,
                include_locks,
            );
            if depth_exceeded {
                filter_depth_limit_hit.store(true, Ordering::Relaxed);
            }
            let is_directory = entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir());
            let should_process =
                should_yield && !filter_ignore_matcher.is_ignored(entry.path(), is_directory);
            if !should_process {
                filter_ignored_entries.fetch_add(1, Ordering::Relaxed);
            }
            if examined >= remaining_entries {
                filter_entry_limit_hit.store(true, Ordering::Relaxed);
                filter_limit_entry_should_process.store(should_process, Ordering::Relaxed);
                // Force the budget-consuming entry to be yielded so the
                // iterator can be dropped before it examines another entry.
                return true;
            }
            should_process
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
        let should_process = !entry_limit_hit.load(Ordering::Relaxed)
            || limit_entry_should_process.load(Ordering::Relaxed);
        let is_directory = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
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
            recover_scan_result(
                scan_file(repo_root, entry.path(), accumulator, true),
                accumulator,
            );
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
    accumulator.record_ignored_entries(ignored_entries.load(Ordering::Relaxed));
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
    root: &Path,
    walk_limits: SourceWalkLimits,
    include_generated: bool,
    include_vendor: bool,
    include_locks: bool,
) -> (bool, bool) {
    let relative_path = entry
        .path()
        .strip_prefix(root)
        .unwrap_or_else(|_| entry.path());
    let is_directory = entry
        .file_type()
        .is_some_and(|file_type| file_type.is_dir());
    if entry.depth() > 0
        && is_directory
        && !should_descend_source_path(relative_path, include_generated, include_vendor)
    {
        return (false, false);
    }
    if !is_directory
        && entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        && !should_scan_source_file(
            relative_path,
            include_generated,
            include_vendor,
            include_locks,
        )
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
    already_filtered: bool,
) -> anyhow::Result<()> {
    let scan_started = Instant::now();
    let result = scan_file_inner(repo_root, path, accumulator, already_filtered);
    accumulator.record_file_scan_match_duration(scan_started.elapsed());
    result
}

fn scan_file_inner(
    repo_root: &Path,
    path: &Path,
    accumulator: &mut SourceSearchAccumulator,
    already_filtered: bool,
) -> anyhow::Result<()> {
    let path = resolve_confined_path(repo_root, path, "source file")?;
    let mut file = open_confined_file(repo_root, &path)?;
    let metadata = file.metadata()?;
    let file_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let relative_path = path.strip_prefix(repo_root).unwrap_or(&path);
    let should_read = if already_filtered {
        accumulator.consider_walked_file(file_len)
    } else {
        accumulator.consider_file(relative_path, file_len)
    };
    if !should_read {
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

struct MatchParameters<'a> {
    case_sensitive: bool,
    context_lines: usize,
    query: &'a str,
    query_cmp: &'a str,
    source_revision: &'a str,
}

fn collect_matches(
    relative_path: &Path,
    text: &str,
    parameters: MatchParameters<'_>,
    state: &mut SearchState,
) {
    let lines = text.lines().collect::<Vec<_>>();
    let relative_path = relative_path.to_string_lossy().replace('\\', "/");
    for (index, line) in lines.iter().enumerate() {
        let is_match = if parameters.case_sensitive {
            line.contains(parameters.query_cmp)
        } else {
            unicode_case_fold(line).contains(parameters.query_cmp)
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
        let start = index.saturating_sub(parameters.context_lines);
        let end = index
            .saturating_add(parameters.context_lines)
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
            id: format!(
                "match:{}",
                &format!(
                    "{:x}",
                    Sha256::digest(
                        format!(
                            "{}\0{relative_path}\0{}\0{}\0{}",
                            parameters.query,
                            parameters.source_revision,
                            index + 1,
                            line,
                        )
                        .as_bytes(),
                    )
                )[..16]
            ),
            file_id: format!(
                "file:{}",
                &format!(
                    "{:x}",
                    Sha256::digest(
                        format!("{relative_path}\0{}", parameters.source_revision).as_bytes()
                    )
                )[..16]
            ),
            path: relative_path.clone(),
            source_revision: parameters.source_revision.to_string(),
            source_map_route: source_map_route_for_path(Path::new(&relative_path)),
            line_number: index + 1,
            matched_content: (*line).to_string(),
            start_line: start + 1,
            end_line: end,
            context_complete: true,
            lines: source_lines,
        };
        state.matches.push(source_match);
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
