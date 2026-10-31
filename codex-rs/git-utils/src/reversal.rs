//! Best-effort textual reversal hints, never patch admission or correctness proof.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::historical::ReadCancellation;
use crate::historical::read_git;
use crate::historical::relative_git_path;
use crate::historical::resolve_read_revision;
use crate::historical::valid_oid;
use crate::inspection::repository_root;
use crate::operations::GitCommandControl;

pub struct ReversalCandidate {
    pub path: PathBuf,
    /// Newly introduced nonblank lines, with one-based post-edit coordinates.
    pub lines: Vec<(usize, String)>,
}

#[derive(Serialize)]
pub struct ReversalMatch {
    pub line: usize,
    pub removed_in_commit: String,
    pub removed_line: usize,
}

#[derive(Serialize)]
pub struct HistoricalReversalWarning {
    pub kind: &'static str,
    pub advisory_only: bool,
    pub path: PathBuf,
    pub matches: Vec<ReversalMatch>,
    pub matches_omitted: usize,
}

#[derive(Default, Serialize)]
pub struct HistoricalReversalCheck {
    pub head: Option<String>,
    pub warnings: Vec<HistoricalReversalWarning>,
    pub skipped: Vec<String>,
}

/// At most eight paths, 32 path-relevant first-parent commits within 30 days,
/// and 1 MiB of Git output per path. The caller supplies an overall time budget.
/// Dropping the future cancels the existing bounded Git process owner.
pub async fn historical_reversal_warnings(
    cwd: PathBuf,
    candidates: Vec<ReversalCandidate>,
) -> Result<HistoricalReversalCheck, String> {
    let control = GitCommandControl::new();
    let _cancellation = ReadCancellation(control.clone());
    tokio::task::spawn_blocking(move || {
        let root = repository_root(&cwd, &control)?;
        let head = resolve_read_revision(&root, "HEAD", &control)?;
        let mut result = HistoricalReversalCheck {
            head: Some(head.clone()),
            ..Default::default()
        };
        for (index, candidate) in candidates.into_iter().enumerate() {
            let path = candidate
                .path
                .strip_prefix(&root)
                .ok()
                .and_then(|path| path.to_str())
                .and_then(|path| relative_git_path(path).ok());
            let Some(path) = path.filter(|_| index < 8 && candidate.lines.len() <= 4096) else {
                result
                    .skipped
                    .push(candidate.path.to_string_lossy().into_owned());
                continue;
            };
            let log = read_git(
                &root,
                &[
                    "--no-pager",
                    "--literal-pathspecs",
                    "log",
                    "--first-parent",
                    "--diff-merges=first-parent",
                    "--max-count=32",
                    "--since=30.days.ago",
                    "--format=%x1e%H",
                    "--patch",
                    "--unified=0",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--no-notes",
                    "--no-show-signature",
                    &head,
                    "--",
                    &path,
                ],
                1024 * 1024,
                &control,
            );
            let Ok(log) = log else {
                result.skipped.push(path);
                continue;
            };
            let removed = removed_lines(&log);
            let mut matches = Vec::new();
            let mut omitted = 0;
            for (line, text) in candidate.lines {
                if let Some((commit, removed_line)) = removed.get(text.trim()) {
                    if matches.len() < 16 {
                        matches.push(ReversalMatch {
                            line,
                            removed_in_commit: commit.clone(),
                            removed_line: *removed_line,
                        });
                    } else {
                        omitted += 1;
                    }
                }
            }
            if !matches.is_empty() {
                result.warnings.push(HistoricalReversalWarning {
                    kind: "historical_reversal",
                    advisory_only: true,
                    path: candidate.path,
                    matches,
                    matches_omitted: omitted,
                });
            }
        }
        Ok(result)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn removed_lines(log: &str) -> BTreeMap<String, (String, usize)> {
    let mut removed = BTreeMap::new();
    let mut commit = "";
    let mut old_line = None;
    for line in log.lines() {
        if let Some(oid) = line.strip_prefix('\u{1e}') {
            commit = if valid_oid(oid) { oid } else { "" };
            old_line = None;
        } else if line.starts_with("diff --git ") {
            old_line = None;
        } else if let Some(header) = line.strip_prefix("@@ -") {
            old_line = header
                .split([',', ' '])
                .next()
                .and_then(|n| n.parse::<usize>().ok());
        } else if let Some(number) = old_line {
            // Inside a hunk, even ---/+++ are source lines, not file headers.
            if let Some(text) = line.strip_prefix('-') {
                let text = text.trim();
                if !commit.is_empty() && text.chars().any(char::is_alphanumeric) {
                    removed
                        .entry(text.to_owned())
                        .or_insert_with(|| (commit.to_owned(), number));
                }
                old_line = number.checked_add(1);
            } else if line.starts_with(' ') {
                old_line = number.checked_add(1);
            } else if !line.starts_with(['+', '\\']) {
                old_line = None;
            }
        }
    }
    removed
}
