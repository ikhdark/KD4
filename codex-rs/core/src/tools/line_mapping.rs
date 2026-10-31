//! Hash-bound, secondary line alignment over complete retained snapshots.

use super::ReadToolOutputError;
use super::ToolOutputSnapshot;
use super::load_tool_output_snapshot;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_SOURCE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_REQUEST_LINES: usize = 1000;
const MAX_SOURCE_LINES: usize = 20_000;

#[derive(Serialize)]
pub(crate) struct LineMapSource {
    artifact_id: String,
    canonical_sha256: String,
    canonical_bytes: u64,
    total_lines: usize,
}

#[derive(Serialize)]
pub(crate) struct LineMapping {
    before_line: usize,
    after_line: Option<usize>,
    status: &'static str,
    reason: &'static str,
}

#[derive(Serialize)]
pub(crate) struct LineMap {
    before: LineMapSource,
    after: LineMapSource,
    mappings: Vec<LineMapping>,
    semantics: &'static str,
    limitations: [&'static str; 2],
}

fn refused(message: impl Into<String>) -> ReadToolOutputError {
    ReadToolOutputError::Io(message.into())
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ReadToolOutputError> {
    if cancellation.is_cancelled() {
        Err(refused("line map cancelled; no mapping delivered"))
    } else {
        Ok(())
    }
}

async fn load_source(
    home: &Path,
    thread: &str,
    artifact_id: &str,
    expected_sha256: &str,
) -> Result<Arc<ToolOutputSnapshot>, ReadToolOutputError> {
    let snapshot = load_tool_output_snapshot(home, thread, artifact_id).await?;
    let meta = &snapshot.metadata;
    if snapshot.historical_authenticity() != "authenticated"
        || !meta.capture_complete
        || !meta.complete
        || !meta.unavailable_ranges.is_empty()
        || meta.canonical_bytes != meta.retained_bytes
        || meta.canonical_bytes != snapshot.bytes.len() as u64
    {
        return Err(refused(
            "line maps require authenticated complete captures without gaps",
        ));
    }
    if !meta.canonical_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(refused(
            "line map SHA-256 mismatch; use the exact before/after canonical hashes",
        ));
    }
    if snapshot.bytes.len() > MAX_SOURCE_BYTES {
        return Err(refused(
            "line map source exceeds 1 MiB; no partial map delivered",
        ));
    }
    Ok(snapshot)
}

fn source(snapshot: &ToolOutputSnapshot, total_lines: usize) -> LineMapSource {
    LineMapSource {
        artifact_id: snapshot.metadata.artifact_id.clone(),
        canonical_sha256: snapshot.metadata.canonical_sha256.clone(),
        canonical_bytes: snapshot.metadata.canonical_bytes,
        total_lines,
    }
}

pub(crate) async fn map_lines(
    home: &Path,
    thread: &str,
    before_id: &str,
    after_id: &str,
    before_sha256: &str,
    after_sha256: &str,
    start_line: usize,
    end_line: usize,
    cancellation: &CancellationToken,
) -> Result<LineMap, ReadToolOutputError> {
    if start_line == 0 || end_line < start_line || end_line - start_line >= MAX_REQUEST_LINES {
        return Err(refused(
            "request an inclusive before-line range of 1..1000 lines",
        ));
    }
    for hash in [before_sha256, after_sha256] {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(refused(
                "before_sha256 and after_sha256 must be 64 hexadecimal digits",
            ));
        }
    }
    check_cancelled(cancellation)?;
    let before = load_source(home, thread, before_id, before_sha256).await?;
    check_cancelled(cancellation)?;
    let after = load_source(home, thread, after_id, after_sha256).await?;
    check_cancelled(cancellation)?;
    let cancellation = cancellation.clone();
    // Keep bounded diff CPU work off the async executor. Cancellation discards
    // its result; the diff timeout bounds the non-interruptible portion.
    tokio::task::spawn_blocking(move || {
        check_cancelled(&cancellation)?;
        let old = std::str::from_utf8(&before.bytes)
            .map_err(|_| refused("before source is not UTF-8"))?;
        let new =
            std::str::from_utf8(&after.bytes).map_err(|_| refused("after source is not UTF-8"))?;
        // LF-only splitting preserves CRLF, lone CR and final-terminator bytes.
        let old_lines: Vec<_> = old.split_inclusive('\n').collect();
        let new_lines: Vec<_> = new.split_inclusive('\n').collect();
        if old_lines.len() > MAX_SOURCE_LINES || new_lines.len() > MAX_SOURCE_LINES {
            return Err(refused("line map source exceeds 20000 lines"));
        }
        if end_line > old_lines.len() {
            return Err(refused("before-line range extends past EOF"));
        }
        let mut old_counts = HashMap::<&str, usize>::new();
        let mut new_counts = HashMap::<&str, usize>::new();
        for line in &old_lines {
            *old_counts.entry(line).or_default() += 1;
        }
        for line in &new_lines {
            *new_counts.entry(line).or_default() += 1;
        }
        let mut config = similar::TextDiff::configure();
        config.timeout(std::time::Duration::from_millis(100));
        // diff_slices uses our LF-only coordinates, not the library's broader
        // newline tokenizer. A timeout may coarsen the diff, never invent equality.
        let diff = config.diff_slices(&old_lines, &new_lines);
        let mut mappings = Vec::with_capacity(end_line - start_line + 1);
        for change in diff.iter_all_changes() {
            check_cancelled(&cancellation)?;
            let Some(index) = change.old_index() else {
                continue;
            };
            if index + 1 < start_line || index + 1 > end_line {
                continue;
            }
            let line = old_lines[index];
            let duplicate = old_counts[line] > 1 || new_counts.get(line).copied().unwrap_or(0) > 1;
            let equal = change.tag() == similar::ChangeTag::Equal;
            let (status, reason, after_line) = if equal && !duplicate {
                (
                    "mapped",
                    "unique_equal_line_in_ordered_diff",
                    change.new_index().map(|i| i + 1),
                )
            } else if equal || new_counts.contains_key(line) {
                ("ambiguous", "duplicate_or_unaligned_equal_text", None)
            } else {
                ("unmapped", "deleted_or_replaced", None)
            };
            mappings.push(LineMapping {
                before_line: index + 1,
                after_line,
                status,
                reason,
            });
        }
        check_cancelled(&cancellation)?;
        Ok(LineMap {
            before: source(&before, old_lines.len()),
            after: source(&after, new_lines.len()),
            mappings,
            semantics: "exact_text_alignment_only",
            limitations: [
                "not_moved_code_identity",
                "not_current_filesystem_freshness",
            ],
        })
    })
    .await
    .map_err(|error| refused(format!("line mapping failed: {error}")))?
}
