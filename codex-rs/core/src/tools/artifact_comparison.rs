//! Bounded byte comparison of retained evidence, not model-visible previews.
//! Reuses the snapshot owner's authentication and thread confinement.

use super::ReadToolOutputError;
use super::ReadToolOutputResult;
use super::ToolOutputSelector;
use super::capture;
use super::load_tool_output_snapshot;
use base64::Engine as _;
use serde::Serialize;
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub(crate) const DEFAULT_COMPARISON_BYTES: usize = 64 * 1024;
pub(crate) const MAX_COMPARISON_BYTES: usize = 1024 * 1024;
const MAX_DIFFERENCE_RANGES: usize = 32;

#[derive(Serialize)]
pub(crate) struct ComparisonSource {
    artifact_id: String,
    canonical_sha256: String,
    canonical_bytes: u64,
    retained_bytes: u64,
    capture_complete: bool,
    historical_authenticity: &'static str,
    available_prefix_bytes: u64,
}

struct Input {
    source: ComparisonSource,
    prefix: Vec<u8>,
    // Every compared prefix byte is producer output. An incomplete capture with
    // no located gap (for example head/tail command output with an inline
    // omission marker) can differ from a full capture of identical output.
    exact_prefix: bool,
}

#[derive(Serialize)]
pub(crate) struct DifferenceRange {
    start: u64,
    end: u64,
}

#[derive(Serialize)]
pub(crate) struct ArtifactComparison {
    pub verdict: &'static str,
    left: ComparisonSource,
    right: ComparisonSource,
    compared_bytes: u64,
    comparison_complete: bool,
    observed_different_bytes: u64,
    difference_ranges: Vec<DifferenceRange>,
    difference_ranges_complete: bool,
    length_difference: bool,
    limitations: Vec<&'static str>,
}

async fn load_input(
    home: &Path,
    thread: &str,
    artifact_id: &str,
    max_bytes: usize,
) -> Result<Input, ReadToolOutputError> {
    if let Some(value) = capture::read_command_log(
        home,
        thread,
        artifact_id,
        &[ToolOutputSelector::Bytes {
            start: 0,
            end: max_bytes as u64,
        }],
        max_bytes,
    )
    .await?
    {
        let output: ReadToolOutputResult = serde_json::from_value(value).map_err(|error| {
            ReadToolOutputError::Io(format!("invalid command comparison input: {error}"))
        })?;
        let row = output.results.first().ok_or_else(|| {
            ReadToolOutputError::Io("command comparison input has no byte selection".into())
        })?;
        let prefix = if let Some(text) = &row.text {
            text.as_bytes().to_vec()
        } else if let Some(encoded) = &row.data_base64 {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| {
                    ReadToolOutputError::Io(format!("invalid command bytes: {error}"))
                })?
        } else {
            return Err(ReadToolOutputError::Io(
                "command comparison input has no bytes".into(),
            ));
        };
        if row
            .canonical_range
            .is_none_or(|range| range.start != 0 || range.end != prefix.len() as u64)
        {
            return Err(ReadToolOutputError::Io(
                "command comparison byte coordinates do not match".into(),
            ));
        }
        return Ok(Input {
            exact_prefix: output.capture_complete,
            source: ComparisonSource {
                artifact_id: output.artifact_id,
                canonical_sha256: output.canonical_sha256,
                canonical_bytes: output.canonical_bytes,
                retained_bytes: output.retained_bytes,
                capture_complete: output.capture_complete,
                // The command-log reader does not authenticate producer-time identity.
                historical_authenticity: "unverified",
                available_prefix_bytes: output.retained_bytes,
            },
            prefix,
        });
    }
    let snapshot = load_tool_output_snapshot(home, thread, artifact_id).await?;
    let metadata = &snapshot.metadata;
    // Never compare a missing span or treat it as a deletion. Only the exact
    // contiguous canonical prefix before the first gap is comparable here.
    let available = metadata
        .unavailable_ranges
        .iter()
        .map(|range| range.start)
        .min()
        .unwrap_or(snapshot.bytes.len() as u64)
        .min(snapshot.bytes.len() as u64);
    Ok(Input {
        // The producer reported a complete capture, so retained bytes before
        // the first located gap (retention or recorded loss) are its output.
        exact_prefix: metadata.version != 0 && metadata.capture_complete,
        source: ComparisonSource {
            artifact_id: metadata.artifact_id.clone(),
            canonical_sha256: metadata.canonical_sha256.clone(),
            canonical_bytes: metadata.canonical_bytes,
            retained_bytes: metadata.retained_bytes,
            capture_complete: metadata.version != 0
                && metadata.capture_complete
                && metadata.complete
                && metadata.unavailable_ranges.is_empty()
                && metadata.canonical_bytes == metadata.retained_bytes,
            historical_authenticity: snapshot.historical_authenticity(),
            available_prefix_bytes: available,
        },
        prefix: snapshot.bytes[..available.min(max_bytes as u64) as usize].to_vec(),
    })
}

pub(crate) async fn compare_artifacts(
    home: &Path,
    thread: &str,
    left_id: &str,
    right_id: &str,
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<ArtifactComparison, ReadToolOutputError> {
    let cancelled =
        || ReadToolOutputError::Io("artifact comparison cancelled; no verdict delivered".into());
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let left = load_input(home, thread, left_id, max_bytes).await?;
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let right = load_input(home, thread, right_id, max_bytes).await?;
    let compared = left.prefix.len().min(right.prefix.len());
    let mut different = 0;
    let mut ranges: Vec<DifferenceRange> = Vec::new();
    let mut ranges_complete = true;
    let mut previous_different = false;
    for (index, (a, b)) in left.prefix.iter().zip(&right.prefix).enumerate() {
        if index % 4096 == 0 && cancellation.is_cancelled() {
            return Err(cancelled());
        }
        if a != b {
            different += 1;
            if previous_different {
                if ranges_complete && let Some(range) = ranges.last_mut() {
                    range.end = index as u64 + 1;
                }
            } else if ranges.len() < MAX_DIFFERENCE_RANGES {
                ranges.push(DifferenceRange {
                    start: index as u64,
                    end: index as u64 + 1,
                });
            } else {
                ranges_complete = false;
            }
        }
        previous_different = a != b;
    }
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let authenticated = left.source.historical_authenticity == "authenticated"
        && right.source.historical_authenticity == "authenticated";
    let captures_complete = left.source.capture_complete && right.source.capture_complete;
    let length_difference = authenticated
        && captures_complete
        && left.source.canonical_bytes != right.source.canonical_bytes;
    let comparison_complete = captures_complete
        && left.prefix.len() as u64 == left.source.canonical_bytes
        && right.prefix.len() as u64 == right.source.canonical_bytes;
    // A mismatch proves different producer output only where both compared
    // prefixes are producer bytes; an unlocated omission stays inconclusive.
    let mismatch_evidence = different > 0 && left.exact_prefix && right.exact_prefix;
    let verdict = if authenticated && (mismatch_evidence || length_difference) {
        "different"
    } else if authenticated && comparison_complete {
        "equal"
    } else {
        "inconclusive"
    };
    let mut limitations = Vec::new();
    if !captures_complete {
        limitations.push("incomplete_capture");
    }
    if !authenticated {
        limitations.push("unverified_origin");
    }
    if left.source.available_prefix_bytes > max_bytes as u64
        || right.source.available_prefix_bytes > max_bytes as u64
    {
        limitations.push("comparison_limit");
    }
    Ok(ArtifactComparison {
        verdict,
        left: left.source,
        right: right.source,
        compared_bytes: compared as u64,
        comparison_complete,
        observed_different_bytes: different,
        difference_ranges: ranges,
        difference_ranges_complete: ranges_complete,
        length_difference,
        limitations,
    })
}
