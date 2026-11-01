//! Exact artifact recovery coverage, independent of cell and dispatch ownership.

use serde_json::Value as JsonValue;

#[derive(Default)]
pub(super) struct DeliveryArtifactCoverage {
    pub(super) sha256: String,
    pub(super) bytes: u64,
    pub(super) ranges: Vec<(u64, u64)>,
    // Empty means the whole canonical artifact. Otherwise only these requested
    // selections are required; unrelated source bytes are not an obligation.
    pub(super) required_ranges: Vec<(u64, u64)>,
}

impl DeliveryArtifactCoverage {
    fn missing_ranges(&self, limit: usize) -> (Vec<(u64, u64)>, bool) {
        let mut required = if self.required_ranges.is_empty() {
            vec![(0, self.bytes)]
        } else {
            self.required_ranges.clone()
        };
        required.sort_unstable();
        let mut missing = Vec::new();
        let mut required_end = 0;
        for (start, end) in required {
            let mut cursor = start.max(required_end);
            required_end = required_end.max(end);
            for &(covered_start, covered_end) in &self.ranges {
                if cursor >= end {
                    break;
                }
                if covered_end <= cursor {
                    continue;
                }
                if covered_start > cursor {
                    missing.push((cursor, covered_start.min(end)));
                    if missing.len() > limit {
                        missing.truncate(limit);
                        return (missing, true);
                    }
                }
                cursor = cursor.max(covered_end);
            }
            if cursor < end {
                missing.push((cursor, end));
                if missing.len() > limit {
                    missing.truncate(limit);
                    return (missing, true);
                }
            }
        }
        (missing, false)
    }

    pub(super) fn recovered(&self) -> bool {
        if self.required_ranges.is_empty() {
            self.bytes == 0
                || self
                    .ranges
                    .first()
                    .is_some_and(|range| *range == (0, self.bytes))
        } else {
            self.required_ranges.iter().all(|(start, end)| {
                start == end
                    || self
                        .ranges
                        .iter()
                        .any(|range| range.0 <= *start && *end <= range.1)
            })
        }
    }

    pub(super) fn observe(&mut self, evidence: &JsonValue) {
        if evidence["identity"]["sha256"].as_str() != Some(self.sha256.as_str()) {
            return;
        }
        // Parsed values preserve semantic data, not exact source coverage:
        // duplicate keys, escaping and whitespace may have been discarded.
        let ranges = evidence["identity"]["ranges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|range| Some((range[0].as_u64()?, range[1].as_u64()?)))
            .collect::<Vec<_>>();
        self.cover(ranges);
    }

    /// Records exact byte ranges already delivered from this artifact.
    pub(super) fn cover(&mut self, ranges: impl IntoIterator<Item = (u64, u64)>) {
        let bytes = self.bytes;
        self.ranges.extend(
            ranges
                .into_iter()
                .filter(|(start, end)| start <= end && *end <= bytes),
        );
        self.ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        self.ranges = merged;
    }
}

pub(super) fn delivery_recovery_refusal<'a>(
    obligations: impl Iterator<Item = (&'a String, &'a DeliveryArtifactCoverage)>,
) -> JsonValue {
    const MAX_RECOVERY_ITEMS: usize = 8;
    const MAX_RECOVERY_RANGES: usize = 8;
    let mut pending = obligations
        .filter(|(_, coverage)| !coverage.recovered())
        .collect::<Vec<_>>();
    pending.sort_by(|(left_id, left), (right_id, right)| {
        (left_id, &left.sha256, &left.required_ranges).cmp(&(
            right_id,
            &right.sha256,
            &right.required_ranges,
        ))
    });
    let omitted_recovery_count = pending.len().saturating_sub(MAX_RECOVERY_ITEMS);
    let recovery = pending
        .into_iter()
        .take(MAX_RECOVERY_ITEMS)
        .map(|(id, coverage)| {
            let (ranges, more_ranges) = coverage.missing_ranges(MAX_RECOVERY_RANGES);
            let selectors = ranges
                .into_iter()
                .map(|(start, end)| serde_json::json!({"kind":"bytes", "start":start, "end":end}))
                .collect::<Vec<_>>();
            serde_json::json!({
                "artifact_id": id,
                "canonical_sha256": coverage.sha256,
                "canonical_bytes": coverage.bytes,
                "tool": "read_tool_output",
                "arguments": {"artifact_id": id, "selectors": selectors},
                "more_missing_ranges": more_ranges,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "category": "evidence_recovery_pending",
        "recovery": recovery,
        "omitted_recovery_count": omitted_recovery_count,
    })
}
