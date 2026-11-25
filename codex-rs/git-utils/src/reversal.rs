//! Best-effort textual reversal hints, never patch admission or correctness proof.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;
use similar::TextDiff;

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
    /// Bounded snapshots for precise attribution; loose matching needs only lines.
    pub text: Option<(String, String)>,
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

#[derive(Serialize)]
pub struct ExactReversal {
    pub path: PathBuf,
    pub commit: String,
    pub line: usize,
}

#[derive(Default, Serialize)]
pub struct HistoricalReversalCheck {
    pub head: Option<String>,
    pub warnings: Vec<HistoricalReversalWarning>,
    pub exact: Vec<ExactReversal>,
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
                    "--full-index",
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
            if let Some((old, new)) = candidate.text {
                result.exact.extend(
                    exact_latest_reversals(&log, &old, &new)
                        .into_iter()
                        .map(|(commit, line)| ExactReversal { path: candidate.path.clone(), commit, line }),
                );
            }
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

// A byte-identical block elsewhere in the file is not evidence of a
// reversal. Bind the preimage to the latest content change for this path;
// otherwise leave attribution to the existing coarse advisory.
fn exact_latest_reversals(log: &str, old: &str, new: &str) -> Vec<(String, usize)> {
    let mut commit = "";
    let mut content_commit = "";
    let mut start = None;
    let mut before = String::new();
    let mut after = String::new();
    let mut removed = false;
    let mut inverses = BTreeMap::new();
    let mut matches = Vec::new();
    for line in log.split_inclusive('\n').chain(std::iter::once("")) {
        let hunk = line.strip_prefix("@@ -");
        if let Some(index) = start
            && (hunk.is_some() || !line.starts_with(['-', '+', '\\']))
        {
            if let Some((wanted_before, wanted_after, post_line)) = inverses.remove(&index)
                && before == wanted_before
                && after == wanted_after
            {
                matches.push((content_commit.to_owned(), post_line));
                if matches.len() == 16 {
                    break;
                }
            }
            start = None;
        }
        if let Some(oid) = line.strip_prefix('\u{1e}').map(str::trim_end) {
            if !content_commit.is_empty() {
                break;
            }
            commit = if valid_oid(oid) { oid } else { "" };
        } else if content_commit.is_empty()
            && !commit.is_empty()
            && let Some((_, right)) = line.strip_prefix("index ").and_then(|s| s.split_once(".."))
            && let Some(oid) = right.split_whitespace().next()
        {
            if !matches_blob(oid, old) {
                break;
            }
            content_commit = commit;
            let diff = TextDiff::from_lines(old, new);
            for op in diff
                .ops()
                .iter()
                .filter(|op| op.tag() == similar::DiffTag::Replace)
            {
                let (mut added, mut removed) = (String::new(), String::new());
                for change in diff.iter_changes(op) {
                    match change.tag() {
                        similar::ChangeTag::Insert => added.push_str(change.value()),
                        similar::ChangeTag::Delete => removed.push_str(change.value()),
                        similar::ChangeTag::Equal => {}
                    }
                }
                inverses.insert(op.old_range().start + 1, (added, removed, op.new_range().start + 1));
            }
        } else if let Some(hunk) = hunk.filter(|_| !content_commit.is_empty()) {
            before.clear();
            after.clear();
            start = hunk
                .split_once(" +")
                .and_then(|(_, range)| range.split([',', ' ']).next())
                .and_then(|number| number.parse::<usize>().ok());
        } else if start.is_some() {
            if let Some(text) = line.strip_prefix('-') {
                before.push_str(text);
                removed = true;
            } else if let Some(text) = line.strip_prefix('+') {
                after.push_str(text);
                removed = false;
            } else if line.starts_with("\\ No newline at end of file") {
                let side = if removed { &mut before } else { &mut after };
                if side.ends_with('\n') {
                    side.pop();
                }
            }
        }
    }
    matches
}

fn matches_blob(oid: &str, text: &str) -> bool {
    use sha2::Digest;

    fn hash<D: Digest>(header: &str, text: &str) -> String {
        let mut digest = D::new();
        digest.update(header.as_bytes());
        digest.update(text.as_bytes());
        digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
    }

    let header = format!("blob {}\0", text.len());
    match oid.len() {
        40 => hash::<sha1::Sha1>(&header, text) == oid,
        64 => hash::<sha2::Sha256>(&header, text) == oid,
        _ => false,
    }
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
