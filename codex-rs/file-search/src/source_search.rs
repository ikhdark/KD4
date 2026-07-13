use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::bail;
use clap::ArgAction;
use clap::Parser;
use ignore::WalkBuilder;
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
    #[arg(long, default_value_t = 1)]
    pub start_line: usize,

    /// Number of lines for --read-file.
    #[arg(long, default_value_t = SOURCE_READ_DEFAULT_LINES)]
    pub line_count: usize,

    /// Maximum number of search matches to return.
    #[arg(long, default_value_t = SOURCE_SEARCH_DEFAULT_MAX_MATCHES)]
    pub max_matches: usize,

    /// Context lines around each search match.
    #[arg(long, default_value_t = 0)]
    pub context_lines: usize,

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
        if cli.query.is_some() || !cli.roots.is_empty() {
            bail!("--read-file cannot be combined with a query or --path");
        }
        let output = read_file_span(ReadFileSpanOptions {
            repo_root: cli.repo_root,
            path,
            start_line: cli.start_line,
            line_count: cli.line_count,
        })?;
        print_output(&output, json, print_span_human)
    } else {
        let Some(query) = cli.query else {
            bail!("a query or --read-file is required");
        };
        let mut options = SourceSearchOptions::new(cli.repo_root, query);
        options.roots = cli.roots;
        options.max_matches = cli.max_matches;
        options.context_lines = cli.context_lines;
        options.case_sensitive = cli.case_sensitive;
        options.include_generated = cli.include_generated;
        options.include_vendor = cli.include_vendor;
        options.include_locks = cli.include_locks;
        let output = search_source(options)?;
        print_output(&output, json, print_search_human)
    }
}

pub fn search_source(options: SourceSearchOptions) -> anyhow::Result<SourceSearchOutput> {
    let repo_root = canonical_repo_root(&options.repo_root)?;
    let roots = resolve_search_roots(&repo_root, &options.roots)?;
    let mut accumulator = SourceSearchAccumulator::new(&options)?;

    for root in &roots {
        if accumulator.should_stop() {
            break;
        }
        scan_root(&repo_root, root, &mut accumulator)?;
    }

    let roots = roots
        .iter()
        .map(|root| relative_display(&repo_root, root))
        .collect();
    Ok(accumulator.finish(roots))
}

pub fn read_file_span(options: ReadFileSpanOptions) -> anyhow::Result<ReadFileSpanOutput> {
    if options.start_line == 0 {
        bail!("start_line must be 1 or greater");
    }
    let repo_root = canonical_repo_root(&options.repo_root)?;
    let path = resolve_confined_path(&repo_root, &options.path, "source file")?;
    let metadata = fs::metadata(&path)
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

    let mut bytes = Vec::with_capacity(file_len);
    File::open(&path)
        .with_context(|| format!("unable to open source file `{}`", path.display()))?
        .take(file_len as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("unable to read source file `{}`", path.display()))?;
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
    if start_line == 0 {
        bail!("start_line must be 1 or greater");
    }
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
    let requested_line_count = line_count.max(1);
    let line_count = requested_line_count.min(SOURCE_READ_MAX_LINES);
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

    let omitted_by_line_limit = requested_line_count > line_count && end_index < source_lines.len();
    Ok(ReadFileSpanOutput {
        path: relative_path.clone(),
        source_map_route: source_map_route_for_path(Path::new(&relative_path)),
        requested_start_line: start_line,
        requested_line_count,
        start_line: lines.first().map(|line| line.line_number),
        end_line: lines.last().map(|line| line.line_number),
        total_lines: source_lines.len(),
        bytes_returned,
        truncated: byte_truncated || omitted_by_line_limit,
        lines,
    })
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
        let max_matches = options.max_matches.clamp(1, SOURCE_SEARCH_MAX_MATCHES);
        let query_cmp = if options.case_sensitive {
            options.query.clone()
        } else {
            unicode_case_fold(&options.query)
        };
        Ok(Self {
            query: options.query.clone(),
            query_cmp,
            case_sensitive: options.case_sensitive,
            context_lines: options.context_lines.min(SOURCE_SEARCH_MAX_CONTEXT_LINES),
            include_generated: options.include_generated,
            include_vendor: options.include_vendor,
            include_locks: options.include_locks,
            state: SearchState::new(max_matches),
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
    files_scanned: usize,
    files_skipped_too_large: usize,
    files_skipped_non_utf8: usize,
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
            files_scanned: 0,
            files_skipped_too_large: 0,
            files_skipped_non_utf8: 0,
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
    filesystem_errors: usize,
    files_skipped_too_large: usize,
    files_skipped_non_utf8: usize,
) -> Option<SourceTruncatedReason> {
    coverage_limit
        .or(result_limit)
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
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .require_git(true)
        .sort_by_file_name(Ord::cmp)
        .filter_entry(move |entry| {
            entry.depth() == 0
                || should_descend_source_path(entry.path(), include_generated, include_vendor)
        });

    for entry in builder.build() {
        if accumulator.should_stop() {
            break;
        }
        let Some(entry) = recover_walk_entry(entry, accumulator) else {
            continue;
        };
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            recover_scan_result(scan_file(repo_root, entry.path(), accumulator), accumulator);
        }
    }
    Ok(())
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
    let metadata = fs::metadata(&path)?;
    let file_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let relative_path = path.strip_prefix(repo_root).unwrap_or(&path);
    if !accumulator.consider_file(relative_path, file_len) {
        return Ok(());
    }

    let mut bytes = Vec::with_capacity(file_len);
    File::open(&path)?
        .take(file_len as u64)
        .read_to_end(&mut bytes)?;
    accumulator.add_file_bytes(relative_path, bytes);
    Ok(())
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
    if is_lockfile(path) && !include_locks {
        return false;
    }
    if !include_vendor && has_named_component(path, is_vendor_dir) {
        return false;
    }
    if !include_generated && has_named_component(path, is_generated_dir) {
        return false;
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
                "cargo.lock" | "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock" | "uv.lock"
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
