use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;

use crate::MAX_SEARCH_RESULTS;
use crate::backend::MemoriesBackendError;
use crate::backend::MemorySearchMatch;
use crate::backend::SearchMatchMode;
use crate::backend::SearchMemoriesRequest;
use crate::backend::SearchMemoriesResponse;

use super::LocalMemoriesBackend;
use super::path::display_relative_path;
use super::path::is_hidden_path;
use super::path::read_sorted_dir_paths;
use super::path::reject_symlink;

const MAX_EXCERPT_BYTES: usize = 16_000;
const MAX_PAGE_CONTENT_BYTES: usize = 80_000;

pub(super) async fn search(
    backend: &LocalMemoriesBackend,
    request: SearchMemoriesRequest,
) -> Result<SearchMemoriesResponse, MemoriesBackendError> {
    let queries = request
        .queries
        .iter()
        .map(|query| query.trim().to_string())
        .collect::<Vec<_>>();
    if queries.is_empty() || queries.iter().any(std::string::String::is_empty) {
        return Err(MemoriesBackendError::EmptyQuery);
    }
    if matches!(
        request.match_mode,
        SearchMatchMode::AllWithinLines { line_count: 0 }
    ) {
        return Err(MemoriesBackendError::InvalidMatchWindow);
    }
    let start_index = match request.cursor.as_deref() {
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            MemoriesBackendError::invalid_cursor(cursor, "must be a non-negative integer")
        })?,
        None => 0,
    };
    let start = backend.resolve_scoped_path(request.path.as_deref()).await?;
    let Some(metadata) = LocalMemoriesBackend::metadata_or_none(&start).await? else {
        return Err(MemoriesBackendError::NotFound {
            path: request.path.unwrap_or_default(),
        });
    };
    reject_symlink(&display_relative_path(&backend.root, &start), &metadata)?;
    let matcher = SearchMatcher::new(
        queries.clone(),
        request.match_mode.clone(),
        request.case_sensitive,
        request.normalized,
    )?;
    let mut files = search_paths(&start, &metadata).await?;
    // Directory traversal order is not global relative-path order (e.g. a.md vs a/x.md).
    files.sort_by_cached_key(|path| display_relative_path(&backend.root, path));
    let mut page = SearchPage {
        skip: start_index,
        limit: request.max_results.clamp(1, MAX_SEARCH_RESULTS),
        matches: Vec::new(),
        remaining_bytes: MAX_PAGE_CONTENT_BYTES,
        has_more: false,
    };
    for path in files {
        search_file(
            &backend.root,
            &path,
            &matcher,
            request.context_lines,
            &mut page,
        )
        .await?;
        if page.has_more {
            break;
        }
    }
    if page.skip != 0 {
        return Err(MemoriesBackendError::invalid_cursor(
            start_index.to_string(),
            "exceeds result count",
        ));
    }
    let next_cursor = page
        .has_more
        .then(|| start_index.saturating_add(page.matches.len()).to_string());
    Ok(SearchMemoriesResponse {
        queries,
        match_mode: request.match_mode,
        path: request.path,
        matches: page.matches,
        next_cursor,
        truncated: page.has_more,
    })
}

async fn search_paths(
    current: &Path,
    metadata: &std::fs::Metadata,
) -> Result<Vec<PathBuf>, MemoriesBackendError> {
    if metadata.is_file() {
        return Ok(vec![current.to_path_buf()]);
    }
    let mut files = Vec::new();
    let mut pending = if metadata.is_dir() {
        vec![current.to_path_buf()]
    } else {
        Vec::new()
    };
    while let Some(dir_path) = pending.pop() {
        let paths = match read_sorted_dir_paths(&dir_path).await {
            Ok(paths) => paths,
            Err(MemoriesBackendError::NotFound { .. }) if dir_path != current => continue,
            Err(err) => return Err(err),
        };
        for path in paths {
            if is_hidden_path(&path) {
                continue;
            }
            let Some(metadata) = LocalMemoriesBackend::metadata_or_none(&path).await? else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

struct SearchPage {
    skip: usize,
    limit: usize,
    matches: Vec<MemorySearchMatch>,
    remaining_bytes: usize,
    has_more: bool,
}

impl SearchPage {
    // Returns false after one lookahead match, without building its excerpt or query strings.
    fn push(&mut self, build: impl FnOnce(usize) -> MemorySearchMatch) -> bool {
        if self.skip > 0 {
            self.skip -= 1;
            return true;
        }
        if self.matches.len() == self.limit || self.remaining_bytes < MAX_EXCERPT_BYTES {
            self.has_more = true;
            return false;
        }
        let found = build(MAX_EXCERPT_BYTES);
        self.remaining_bytes -= found.content.len();
        self.matches.push(found);
        true
    }
}

#[expect(
    clippy::expect_used,
    reason = "Front checks prove nonempty windows; complete query counts retain at least one matching line"
)]
async fn search_file(
    root: &Path,
    path: &Path,
    matcher: &SearchMatcher,
    context_lines: usize,
    page: &mut SearchPage,
) -> Result<(), MemoriesBackendError> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(MemoriesBackendError::NotFound {
                path: display_relative_path(root, path),
            });
        }
        Err(err) => return Err(err.into()),
    };
    let lines = content.lines().collect::<Vec<_>>();
    match &matcher.match_mode {
        SearchMatchMode::Any | SearchMatchMode::AllOnSameLine => {
            for (idx, line) in lines.iter().enumerate() {
                let flags = matcher.matched_query_flags(line);
                let matched = match matcher.match_mode {
                    SearchMatchMode::Any => flags.iter().any(|matched| *matched),
                    _ => flags.iter().all(|matched| *matched),
                };
                if matched
                    && !page.push(|budget| {
                        build_search_match(
                            root,
                            path,
                            &lines,
                            idx,
                            idx,
                            context_lines,
                            matcher.matched_queries(&flags),
                            budget,
                        )
                    })
                {
                    break;
                }
            }
        }
        SearchMatchMode::AllWithinLines { line_count } => {
            let mut counts = vec![0usize; matcher.queries.len()];
            let mut window: VecDeque<(usize, Vec<bool>)> = VecDeque::new();
            for (end, line) in lines.iter().enumerate() {
                // Each matching line enters and leaves the window once. Counts replace
                // rescanning every possible start, and shrinking emits only minimal windows.
                while window
                    .front()
                    .is_some_and(|(start, _)| end - start >= *line_count)
                {
                    let (_, flags) = window.pop_front().expect("nonempty window");
                    for (count, flag) in counts.iter_mut().zip(flags) {
                        *count -= usize::from(flag);
                    }
                }
                let flags = matcher.matched_query_flags(line);
                if !flags.iter().any(|matched| *matched) {
                    continue;
                }
                for (count, flag) in counts.iter_mut().zip(&flags) {
                    *count += usize::from(*flag);
                }
                window.push_back((end, flags));
                if counts.contains(&0) {
                    continue;
                }
                while window.front().is_some_and(|(_, flags)| {
                    flags
                        .iter()
                        .zip(&counts)
                        .all(|(flag, count)| !flag || *count > 1)
                }) {
                    let (_, flags) = window.pop_front().expect("nonempty window");
                    for (count, flag) in counts.iter_mut().zip(flags) {
                        *count -= usize::from(flag);
                    }
                }
                let (start, flags) = window.pop_front().expect("complete window");
                if !page.push(|budget| {
                    build_search_match(
                        root,
                        path,
                        &lines,
                        start,
                        end,
                        context_lines,
                        matcher.queries.clone(),
                        budget,
                    )
                }) {
                    break;
                }
                for (count, flag) in counts.iter_mut().zip(flags) {
                    *count -= usize::from(flag);
                }
            }
        }
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "Render one match with its source span, context, matched queries, and byte budget"
)]
fn build_search_match(
    root: &Path,
    path: &Path,
    lines: &[&str],
    match_start_index: usize,
    match_end_index: usize,
    context_lines: usize,
    matched_queries: Vec<String>,
    max_bytes: usize,
) -> MemorySearchMatch {
    let requested_start = match_start_index.saturating_sub(context_lines);
    let content_end = match_end_index
        .saturating_add(context_lines)
        .saturating_add(1)
        .min(lines.len());
    // Reserve at least half the excerpt for the match instead of filling it with leading context.
    let mut content_start = match_start_index;
    let mut prefix_bytes = 0usize;
    while content_start > requested_start {
        let len = lines[content_start - 1].len().saturating_add(1);
        if len > max_bytes / 2 - prefix_bytes {
            break;
        }
        prefix_bytes += len;
        content_start -= 1;
    }
    let mut content = String::new();
    let mut content_truncated = content_start != requested_start;
    for (idx, line) in lines[content_start..content_end].iter().enumerate() {
        if idx > 0 {
            if content.len() == max_bytes {
                content_truncated = true;
                break;
            }
            content.push('\n');
        }
        let mut end = line.len().min(max_bytes - content.len());
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        content.push_str(&line[..end]);
        if end < line.len() {
            content_truncated = true;
            break;
        }
    }
    MemorySearchMatch {
        path: display_relative_path(root, path),
        match_line_number: match_start_index + 1,
        content_start_line_number: content_start + 1,
        content,
        content_truncated,
        matched_queries,
    }
}

struct SearchMatcher {
    queries: Vec<String>,
    prepared_queries: Vec<String>,
    comparison: SearchComparison,
    match_mode: SearchMatchMode,
}

impl SearchMatcher {
    fn new(
        queries: Vec<String>,
        match_mode: SearchMatchMode,
        case_sensitive: bool,
        normalized: bool,
    ) -> Result<Self, MemoriesBackendError> {
        let comparison = SearchComparison::new(case_sensitive, normalized);
        let prepared_queries = queries
            .iter()
            .map(|query| comparison.prepare(query))
            .map(Cow::into_owned)
            .collect::<Vec<_>>();
        if prepared_queries.iter().any(std::string::String::is_empty) {
            return Err(MemoriesBackendError::EmptyQuery);
        }
        Ok(Self {
            queries,
            prepared_queries,
            comparison,
            match_mode,
        })
    }

    fn matched_query_flags(&self, line: &str) -> Vec<bool> {
        let line = self.comparison.prepare(line);
        self.prepared_queries
            .iter()
            .map(|query| line.as_ref().contains(query))
            .collect()
    }

    fn matched_queries(&self, matched_query_flags: &[bool]) -> Vec<String> {
        self.queries
            .iter()
            .zip(matched_query_flags)
            .filter(|(_, matched)| **matched)
            .map(|(query, _)| query.clone())
            .collect()
    }
}

#[derive(Clone, Copy)]
struct SearchComparison {
    case_sensitive: bool,
    normalized: bool,
}

impl SearchComparison {
    fn new(case_sensitive: bool, normalized: bool) -> Self {
        Self {
            case_sensitive,
            normalized,
        }
    }

    fn prepare<'a>(self, value: &'a str) -> Cow<'a, str> {
        if self.case_sensitive && !self.normalized {
            return Cow::Borrowed(value);
        }

        let value = if self.case_sensitive {
            Cow::Borrowed(value)
        } else {
            Cow::Owned(value.to_lowercase())
        };
        if !self.normalized {
            return value;
        }

        Cow::Owned(
            value
                .chars()
                .filter(|ch| ch.is_alphanumeric())
                .collect::<String>(),
        )
    }
}
