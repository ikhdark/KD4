use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::approx_token_count;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use super::*;
use crate::task_evidence::deterministic_requirement_id;
use crate::task_evidence::material_for_span;

pub(super) const SOURCE_CLASSIFICATION_V2_MARKER: &str = "KD4_SOURCE_CLASSIFICATION_REQUEST_V2";
const LOCAL_STRUCTURAL_REASON: &str =
    "source contains only locally recognized non-requirement Markdown structure";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PartitionKind {
    LockedRequirement,
    Structural,
    Unresolved,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct PartitionSpan {
    start: usize,
    end: usize,
    kind: PartitionKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SourceIdentity {
    source_id: String,
    source_hash: String,
    source_kind: String,
    source_ordinal: u64,
    content_ordinal: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct LockedRequirementIdentity {
    requirement_id: String,
    #[serde(flatten)]
    source: SourceIdentity,
    source_span: WireSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ImmutableContext {
    context_id: String,
    containing_heading: String,
    preceding_locked_entry: Option<LockedRequirementIdentity>,
    following_locked_entry: Option<LockedRequirementIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct UnresolvedRange {
    range_id: String,
    #[serde(flatten)]
    source: SourceIdentity,
    source_span: WireSpan,
    source_order: usize,
    range_order: usize,
    exact_material: String,
    context_id: String,
}

#[derive(Clone, Debug)]
struct SourcePartition {
    source_id: String,
    spans: Vec<PartitionSpan>,
    heading_for_unresolved: BTreeMap<(usize, usize), String>,
}

#[derive(Clone, Debug)]
pub(super) struct ClassificationPlan {
    ranges: Vec<UnresolvedRange>,
    contexts: Vec<ImmutableContext>,
    locked: Vec<LockedRequirementIdentity>,
    locally_locked: BTreeMap<String, Vec<ClassifiedRequirement>>,
    locally_complete: BTreeMap<String, ClassifiedSource>,
}

#[derive(Clone, Debug)]
pub(super) enum ClassificationRoute {
    LocalOnly(Vec<ClassifiedSource>),
    V1,
    V2(ClassificationPlan),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum V2RangeResultKind {
    RequirementBearing,
    NonRequirement,
    SupersededContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct V2RangeClassification {
    range_id: String,
    source_id: String,
    source_hash: String,
    source_kind: String,
    source_ordinal: u64,
    content_ordinal: u64,
    source_span: WireSpan,
    result: V2RangeResultKind,
    requirements: Vec<ClassificationRequirement>,
    reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum V2SourceDispositionKind {
    NonRequirement,
    SupersededContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct V2SourceDisposition {
    source_id: String,
    source_hash: String,
    source_kind: String,
    source_ordinal: u64,
    content_ordinal: u64,
    disposition: V2SourceDispositionKind,
    reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct V2LockedOverlay {
    requirement_id: String,
    source_id: String,
    source_hash: String,
    source_kind: String,
    source_ordinal: u64,
    content_ordinal: u64,
    source_span: WireSpan,
    status: WireRequirementStatus,
    superseded_by: Option<LockedRequirementIdentityWire>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct LockedRequirementIdentityWire {
    requirement_id: String,
    source_id: String,
    source_hash: String,
    source_kind: String,
    source_ordinal: u64,
    content_ordinal: u64,
    source_span: WireSpan,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceClassificationOutputV2 {
    pub(super) range_classifications: Vec<V2RangeClassification>,
    pub(super) source_dispositions: Vec<V2SourceDisposition>,
    pub(super) locked_requirement_overlays: Vec<V2LockedOverlay>,
}

fn source_kind_name(kind: UserSourceKind) -> &'static str {
    match kind {
        UserSourceKind::Text => "text",
        UserSourceKind::Image => "image",
        UserSourceKind::Attachment => "attachment",
    }
}

fn source_identity(source: &UserSourceRecord) -> SourceIdentity {
    SourceIdentity {
        source_id: source.source_id.clone(),
        source_hash: source.content_hash.clone(),
        source_kind: source_kind_name(source.source_kind).to_string(),
        source_ordinal: source.source_ordinal,
        content_ordinal: source.content_ordinal,
    }
}

fn wire_span(source: &UserSourceRecord, span: &SourceSpan) -> WireSpan {
    match span {
        SourceSpan::Text { start, end } => WireSpan {
            kind: "text".to_string(),
            start: *start,
            end: *end,
            reference: String::new(),
            subreference: String::new(),
        },
        SourceSpan::Image { reference, region } => WireSpan {
            kind: "image".to_string(),
            start: 0,
            end: 0,
            reference: if reference == &source.exact_material {
                reviewer_source_reference(source)
            } else {
                reference.clone()
            },
            subreference: region.clone().unwrap_or_default(),
        },
        SourceSpan::Attachment { reference, range } => WireSpan {
            kind: "attachment".to_string(),
            start: 0,
            end: 0,
            reference: reference.clone(),
            subreference: range.clone().unwrap_or_default(),
        },
    }
}

fn locked_identity(
    source: &UserSourceRecord,
    requirement_id: String,
    span: &SourceSpan,
) -> LockedRequirementIdentity {
    LockedRequirementIdentity {
        requirement_id,
        source: source_identity(source),
        source_span: wire_span(source, span),
    }
}

fn classification_for_terminal(
    dossier: &CompletionReviewDossier,
    source: &UserSourceRecord,
    mapping: &SourceMapping,
) -> Option<ClassifiedSource> {
    let source_requirements = dossier
        .requirements
        .iter()
        .filter(|requirement| requirement.source_id == source.source_id)
        .collect::<Vec<_>>();
    let requirements = source_requirements
        .iter()
        .map(|requirement| ClassifiedRequirement {
            source_span: requirement.source_span.clone(),
            status: requirement.status,
            superseded_by: requirement.superseded_by.as_ref().and_then(|target_id| {
                dossier
                    .requirements
                    .iter()
                    .find(|target| &target.requirement_id == target_id)
                    .map(|target| ClassifiedRequirementRef {
                        source_id: target.source_id.clone(),
                        source_span: target.source_span.clone(),
                    })
            }),
        })
        .collect::<Vec<_>>();
    Some(match mapping {
        SourceMapping::PendingClassification => return None,
        SourceMapping::RequirementBearing { requirement_ids } => {
            let mapped_ids = requirement_ids.iter().collect::<BTreeSet<_>>();
            let stored_ids = source_requirements
                .iter()
                .map(|requirement| &requirement.requirement_id)
                .collect::<BTreeSet<_>>();
            if mapped_ids.len() != requirement_ids.len() || mapped_ids != stored_ids {
                return None;
            }
            ClassifiedSource {
                source_id: source.source_id.clone(),
                kind: ClassifiedSourceKind::RequirementBearing,
                requirements,
                reason: None,
            }
        }
        SourceMapping::NonRequirement { reason } => ClassifiedSource {
            source_id: source.source_id.clone(),
            kind: ClassifiedSourceKind::NonRequirement,
            requirements: Vec::new(),
            reason: Some(reason.clone()),
        },
        SourceMapping::SupersededContext { reason } => ClassifiedSource {
            source_id: source.source_id.clone(),
            kind: ClassifiedSourceKind::SupersededContext,
            requirements: Vec::new(),
            reason: Some(reason.clone()),
        },
        SourceMapping::UnavailableOrTruncated => ClassifiedSource {
            source_id: source.source_id.clone(),
            kind: ClassifiedSourceKind::UnavailableOrTruncated,
            requirements: Vec::new(),
            reason: None,
        },
    })
}

pub(super) fn plan_classification(
    dossier: &CompletionReviewDossier,
) -> Option<ClassificationRoute> {
    let mut partitions = Vec::new();
    let mut locked = Vec::new();
    let mut locally_locked = BTreeMap::<String, Vec<ClassifiedRequirement>>::new();
    let mut locally_complete = BTreeMap::<String, ClassifiedSource>::new();
    let mut authoritative = false;

    for source in &dossier.sources {
        let mapping = dossier.source_mappings.get(&source.source_id)?;
        if !matches!(mapping, SourceMapping::PendingClassification) {
            authoritative = true;
            for requirement in dossier
                .requirements
                .iter()
                .filter(|requirement| requirement.source_id == source.source_id)
            {
                locked.push(locked_identity(
                    source,
                    requirement.requirement_id.clone(),
                    &requirement.source_span,
                ));
            }
            continue;
        }
        if source.availability != UserSourceAvailability::Available {
            authoritative = true;
            locally_complete.insert(
                source.source_id.clone(),
                ClassifiedSource {
                    source_id: source.source_id.clone(),
                    kind: ClassifiedSourceKind::UnavailableOrTruncated,
                    requirements: Vec::new(),
                    reason: None,
                },
            );
            continue;
        }
        if source.source_kind != UserSourceKind::Text {
            partitions.push(non_text_partition(source));
            continue;
        }
        let partition = partition_text(source)?;
        let local_requirements = partition
            .spans
            .iter()
            .filter(|span| span.kind == PartitionKind::LockedRequirement)
            .map(|span| {
                let source_span = SourceSpan::Text {
                    start: span.start,
                    end: span.end,
                };
                locked.push(locked_identity(
                    source,
                    deterministic_requirement_id(source, &source_span),
                    &source_span,
                ));
                ClassifiedRequirement {
                    source_span,
                    status: RequirementStatus::Active,
                    superseded_by: None,
                }
            })
            .collect::<Vec<_>>();
        let has_unresolved = partition
            .spans
            .iter()
            .any(|span| span.kind == PartitionKind::Unresolved);
        if !local_requirements.is_empty() {
            authoritative = true;
            locally_locked.insert(source.source_id.clone(), local_requirements.clone());
        }
        if !has_unresolved {
            authoritative = true;
            let kind = if local_requirements.is_empty() {
                ClassifiedSourceKind::NonRequirement
            } else {
                ClassifiedSourceKind::RequirementBearing
            };
            locally_complete.insert(
                source.source_id.clone(),
                ClassifiedSource {
                    source_id: source.source_id.clone(),
                    kind,
                    requirements: local_requirements,
                    reason: (kind == ClassifiedSourceKind::NonRequirement)
                        .then(|| LOCAL_STRUCTURAL_REASON.to_string()),
                },
            );
        }
        partitions.push(partition);
    }

    locked.sort_by_cached_key(locked_order_key);
    let (ranges, contexts) = build_ranges(dossier, &partitions, &locked)?;
    if ranges.is_empty() {
        return Some(ClassificationRoute::LocalOnly(merge_without_model(
            dossier,
            &locally_complete,
        )?));
    }
    if !authoritative {
        return Some(ClassificationRoute::V1);
    }
    Some(ClassificationRoute::V2(ClassificationPlan {
        ranges,
        contexts,
        locked,
        locally_locked,
        locally_complete,
    }))
}

fn non_text_partition(source: &UserSourceRecord) -> SourcePartition {
    SourcePartition {
        source_id: source.source_id.clone(),
        spans: vec![PartitionSpan {
            start: 0,
            end: source.exact_material.len(),
            kind: PartitionKind::Unresolved,
        }],
        heading_for_unresolved: BTreeMap::new(),
    }
}

fn merge_without_model(
    dossier: &CompletionReviewDossier,
    locally_complete: &BTreeMap<String, ClassifiedSource>,
) -> Option<Vec<ClassifiedSource>> {
    dossier
        .sources
        .iter()
        .map(|source| {
            let mapping = dossier.source_mappings.get(&source.source_id)?;
            classification_for_terminal(dossier, source, mapping)
                .or_else(|| locally_complete.get(&source.source_id).cloned())
        })
        .collect()
}

fn locked_order_key(identity: &LockedRequirementIdentity) -> (u64, u64, usize, String) {
    (
        identity.source.source_ordinal,
        identity.source.content_ordinal,
        identity.source_span.start,
        identity.requirement_id.clone(),
    )
}

fn build_ranges(
    dossier: &CompletionReviewDossier,
    partitions: &[SourcePartition],
    locked: &[LockedRequirementIdentity],
) -> Option<(Vec<UnresolvedRange>, Vec<ImmutableContext>)> {
    let mut ranges = Vec::new();
    let mut contexts = Vec::<ImmutableContext>::new();
    let mut context_keys = BTreeMap::<String, String>::new();
    for (source_order, source) in dossier.sources.iter().enumerate() {
        let Some(partition) = partitions
            .iter()
            .find(|partition| partition.source_id == source.source_id)
        else {
            continue;
        };
        for (range_order, span) in partition
            .spans
            .iter()
            .filter(|span| span.kind == PartitionKind::Unresolved)
            .enumerate()
        {
            let source_span = match source.source_kind {
                UserSourceKind::Text => SourceSpan::Text {
                    start: span.start,
                    end: span.end,
                },
                UserSourceKind::Image => SourceSpan::Image {
                    reference: source.exact_material.clone(),
                    region: None,
                },
                UserSourceKind::Attachment => SourceSpan::Attachment {
                    reference: source.exact_material.clone(),
                    range: None,
                },
            };
            let current_key = (
                source.source_ordinal,
                source.content_ordinal,
                span.start,
                String::new(),
            );
            let preceding = locked
                .iter()
                .rfind(|entry| locked_order_key(entry) < current_key)
                .cloned();
            let following = locked
                .iter()
                .find(|entry| locked_order_key(entry) > current_key)
                .cloned();
            let heading = partition
                .heading_for_unresolved
                .get(&(span.start, span.end))
                .cloned()
                .unwrap_or_default();
            let key = serde_json::to_string(&(heading.as_str(), &preceding, &following)).ok()?;
            let context_id = if let Some(id) = context_keys.get(&key) {
                id.clone()
            } else {
                let id = format!("context-{}", contexts.len() + 1);
                contexts.push(ImmutableContext {
                    context_id: id.clone(),
                    containing_heading: heading,
                    preceding_locked_entry: preceding,
                    following_locked_entry: following,
                });
                context_keys.insert(key, id.clone());
                id
            };
            ranges.push(UnresolvedRange {
                range_id: format!("range-{}-{}", source_order + 1, range_order + 1),
                source: source_identity(source),
                source_span: wire_span(source, &source_span),
                source_order,
                range_order,
                exact_material: match source.source_kind {
                    UserSourceKind::Text => material_for_span(source, &source_span)?,
                    UserSourceKind::Image => reviewer_source_reference(source),
                    UserSourceKind::Attachment => source.exact_material.clone(),
                },
                context_id,
            });
        }
    }
    Some((ranges, contexts))
}

#[derive(Clone, Debug)]
struct Line<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

fn lines_with_offsets(text: &str) -> Vec<Line<'_>> {
    let mut result = Vec::new();
    let mut start = 0;
    for inclusive in text.split_inclusive('\n') {
        let end = start + inclusive.len();
        let content_end = if inclusive.ends_with("\r\n") {
            end - 2
        } else if inclusive.ends_with('\n') {
            end - 1
        } else {
            end
        };
        result.push(Line {
            start,
            end,
            text: &text[start..content_end],
        });
        start = end;
    }
    if text.is_empty() {
        return result;
    }
    if start < text.len() {
        result.push(Line {
            start,
            end: text.len(),
            text: &text[start..],
        });
    }
    result
}

fn partition_text(source: &UserSourceRecord) -> Option<SourcePartition> {
    let text = source.exact_material.as_str();
    let lines = lines_with_offsets(text);
    let mut spans = Vec::new();
    let mut headings = BTreeMap::new();
    let mut current_heading = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];
        let trimmed = line.text.trim();
        if trimmed.is_empty() {
            spans.push(part(line.start, line.end, PartitionKind::Structural));
            index += 1;
            continue;
        }
        if let Some(heading) = markdown_heading(trimmed) {
            current_heading = heading.to_string();
            spans.push(part(line.start, line.end, PartitionKind::Structural));
            index += 1;
            continue;
        }
        if is_fence(trimmed) {
            let start = line.start;
            index += 1;
            while index < lines.len() {
                let closes = is_fence(lines[index].text.trim());
                index += 1;
                if closes {
                    break;
                }
            }
            let end = lines[index.saturating_sub(1)].end;
            push_unresolved(&mut spans, &mut headings, start, end, &current_heading);
            continue;
        }
        if line.text.starts_with('>')
            || line.text.starts_with("    ")
            || line.text.starts_with('\t')
        {
            push_unresolved(
                &mut spans,
                &mut headings,
                line.start,
                line.end,
                &current_heading,
            );
            index += 1;
            continue;
        }
        if line.text.contains('|') {
            let start_index = index;
            while index < lines.len() && lines[index].text.contains('|') {
                index += 1;
            }
            if let Some(table_spans) = accepted_table(&lines[start_index..index], &current_heading)
            {
                spans.extend(table_spans);
            } else {
                push_unresolved(
                    &mut spans,
                    &mut headings,
                    lines[start_index].start,
                    lines[index - 1].end,
                    &current_heading,
                );
            }
            continue;
        }
        if let Some((marker_end, content_start, content_end)) = list_item_offsets(line) {
            let block_start = line.start;
            let mut block_end = line.end;
            let mut next = index + 1;
            while next < lines.len() {
                if is_list_continuation(line, &lines[next]) {
                    block_end = lines[next].end;
                    next += 1;
                    continue;
                }
                if lines[next].text.trim().is_empty() {
                    let mut after_blanks = next + 1;
                    while after_blanks < lines.len() && lines[after_blanks].text.trim().is_empty() {
                        after_blanks += 1;
                    }
                    if after_blanks < lines.len()
                        && is_list_continuation(line, &lines[after_blanks])
                    {
                        block_end = lines[after_blanks].end;
                        next = after_blanks + 1;
                        continue;
                    }
                }
                break;
            }
            if next != index + 1 {
                push_unresolved(
                    &mut spans,
                    &mut headings,
                    block_start,
                    block_end,
                    &current_heading,
                );
                index = next;
                continue;
            }
            let material = &text[content_start..content_end];
            if extractable_list_item(material, &current_heading) {
                spans.push(part(line.start, marker_end, PartitionKind::Structural));
                spans.push(part(
                    content_start,
                    content_end,
                    PartitionKind::LockedRequirement,
                ));
                spans.push(part(content_end, line.end, PartitionKind::Structural));
            } else {
                push_unresolved(
                    &mut spans,
                    &mut headings,
                    line.start,
                    line.end,
                    &current_heading,
                );
            }
            index += 1;
            continue;
        }
        push_unresolved(
            &mut spans,
            &mut headings,
            line.start,
            line.end,
            &current_heading,
        );
        index += 1;
    }
    coalesce(&mut spans);
    validate_partition(text, &spans)?;
    Some(SourcePartition {
        source_id: source.source_id.clone(),
        spans,
        heading_for_unresolved: headings,
    })
}

fn part(start: usize, end: usize, kind: PartitionKind) -> PartitionSpan {
    PartitionSpan { start, end, kind }
}

fn push_unresolved(
    spans: &mut Vec<PartitionSpan>,
    headings: &mut BTreeMap<(usize, usize), String>,
    start: usize,
    end: usize,
    heading: &str,
) {
    spans.push(part(start, end, PartitionKind::Unresolved));
    headings.insert((start, end), heading.to_string());
}

fn coalesce(spans: &mut Vec<PartitionSpan>) {
    let mut merged = Vec::<PartitionSpan>::new();
    for span in spans.drain(..) {
        if span.start == span.end {
            continue;
        }
        if let Some(previous) = merged.last_mut()
            && span.kind == PartitionKind::Structural
            && previous.kind == span.kind
            && previous.end == span.start
        {
            previous.end = span.end;
        } else {
            merged.push(span);
        }
    }
    *spans = merged;
}

fn validate_partition(text: &str, spans: &[PartitionSpan]) -> Option<()> {
    let mut cursor = 0;
    for span in spans {
        if span.start != cursor
            || span.start >= span.end
            || span.end > text.len()
            || !text.is_char_boundary(span.start)
            || !text.is_char_boundary(span.end)
        {
            return None;
        }
        cursor = span.end;
    }
    (cursor == text.len()).then_some(())
}

fn markdown_heading(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    (hashes > 0 && hashes <= 6 && trimmed.as_bytes().get(hashes) == Some(&b' '))
        .then(|| trimmed[hashes + 1..].trim_end_matches('#').trim())
}

fn normalized_heading(heading: &str) -> String {
    heading
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || character.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn requirement_heading(heading: &str) -> bool {
    let heading = normalized_heading(heading);
    heading.contains("requirement")
        || heading.contains("acceptance criteria")
        || heading == "constraints"
        || heading == "implementation constraints"
}

fn excluded_heading(heading: &str) -> bool {
    let heading = normalized_heading(heading);
    [
        "background",
        "example",
        "notes",
        "progress",
        "status",
        "updates",
        "work log",
    ]
    .iter()
    .any(|excluded| heading.contains(excluded))
}

fn is_fence(trimmed: &str) -> bool {
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn list_item_offsets(line: &Line<'_>) -> Option<(usize, usize, usize)> {
    let bytes = line.text.as_bytes();
    let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let after_marker = match bytes.get(indent)? {
        b'-' | b'+' | b'*' if bytes.get(indent + 1) == Some(&b' ') => indent + 2,
        byte if byte.is_ascii_digit() => {
            let digits = bytes[indent..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let punctuation = indent + digits;
            if !matches!(bytes.get(punctuation), Some(b'.' | b')'))
                || bytes.get(punctuation + 1) != Some(&b' ')
            {
                return None;
            }
            punctuation + 2
        }
        _ => return None,
    };
    let mut content = after_marker;
    if bytes.get(content) == Some(&b'[')
        && matches!(bytes.get(content + 1), Some(b' ' | b'x' | b'X'))
        && bytes.get(content + 2) == Some(&b']')
        && bytes.get(content + 3) == Some(&b' ')
    {
        content += 4;
    }
    while bytes.get(content) == Some(&b' ') {
        content += 1;
    }
    let mut end = bytes.len();
    while end > content && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    (content < end).then_some((line.start + content, line.start + content, line.start + end))
}

fn is_list_continuation(parent: &Line<'_>, candidate: &Line<'_>) -> bool {
    if candidate.text.trim().is_empty() {
        return false;
    }
    let parent_indent = parent.text.len() - parent.text.trim_start_matches(' ').len();
    let candidate_indent = candidate.text.len() - candidate.text.trim_start_matches(' ').len();
    if candidate_indent > parent_indent {
        return true;
    }
    let trimmed = candidate.text.trim_start();
    markdown_heading(trimmed).is_none()
        && !is_fence(trimmed)
        && !trimmed.starts_with('>')
        && list_item_offsets(candidate).is_none()
}

fn strip_inline_code(material: &str) -> String {
    let mut result = String::with_capacity(material.len());
    let mut in_code = false;
    for character in material.chars() {
        if character == '`' {
            in_code = !in_code;
            result.push(' ');
        } else if in_code {
            result.push(' ');
        } else {
            result.push(character);
        }
    }
    result
}

fn contains_supersession_veto(material: &str) -> bool {
    let lower = strip_inline_code(material).to_ascii_lowercase();
    [
        "instead",
        "replace",
        "no longer",
        "previous",
        "withdraw",
        "supersede",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn clear_directive(material: &str) -> bool {
    let lower = strip_inline_code(material).trim().to_ascii_lowercase();
    [
        "must ",
        "must not ",
        "shall ",
        "required ",
        "need to ",
        "do not ",
        "don't ",
        "never ",
        "ensure ",
    ]
    .iter()
    .any(|needle| lower.starts_with(needle) || lower.contains(&format!(" {needle}")))
        || [
            "add",
            "use",
            "keep",
            "preserve",
            "reject",
            "validate",
            "ensure",
            "support",
            "implement",
            "remove",
            "avoid",
            "treat",
            "make",
            "attach",
            "apply",
            "resolve",
            "send",
            "include",
            "exclude",
            "return",
            "lock",
            "permit",
            "skip",
            "classify",
            "extract",
        ]
        .iter()
        .any(|verb| lower.starts_with(&format!("{verb} ")))
}

fn extractable_list_item(material: &str, heading: &str) -> bool {
    !excluded_heading(heading)
        && !contains_supersession_veto(material)
        && (requirement_heading(heading) || clear_directive(material))
}

fn accepted_table(lines: &[Line<'_>], heading: &str) -> Option<Vec<PartitionSpan>> {
    if lines.len() < 2 || excluded_heading(heading) {
        return None;
    }
    let parsed = lines
        .iter()
        .map(parse_table_row)
        .collect::<Option<Vec<_>>>()?;
    let columns = parsed.first()?.len();
    if columns == 0 || parsed.iter().any(|row| row.len() != columns) {
        return None;
    }
    if !parsed[1]
        .iter()
        .all(|cell| is_separator_cell(cell.material))
    {
        return None;
    }
    let requirement_columns = parsed[0]
        .iter()
        .enumerate()
        .filter(|(_, cell)| requirement_heading(cell.material))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if requirement_columns.len() != 1 {
        return None;
    }
    let requirement_column = requirement_columns[0];
    if parsed.iter().skip(2).any(|row| {
        row.iter().enumerate().any(|(index, cell)| {
            index != requirement_column && !pure_markdown_structure(cell.material)
        }) || contains_supersession_veto(row[requirement_column].material)
    }) {
        return None;
    }
    let mut locked_cells = parsed
        .iter()
        .skip(2)
        .filter_map(|row| {
            let cell = &row[requirement_column];
            (!cell.material.trim().is_empty()).then_some((cell.start, cell.end))
        })
        .collect::<Vec<_>>();
    locked_cells.sort_unstable();
    let table_start = lines.first()?.start;
    let table_end = lines.last()?.end;
    let mut cursor = table_start;
    let mut spans = Vec::new();
    for (start, end) in locked_cells {
        if cursor < start {
            spans.push(part(cursor, start, PartitionKind::Structural));
        }
        spans.push(part(start, end, PartitionKind::LockedRequirement));
        cursor = end;
    }
    if cursor < table_end {
        spans.push(part(cursor, table_end, PartitionKind::Structural));
    }
    Some(spans)
}

#[derive(Clone, Debug)]
struct TableCell<'a> {
    start: usize,
    end: usize,
    material: &'a str,
}

fn parse_table_row<'a>(line: &'a Line<'a>) -> Option<Vec<TableCell<'a>>> {
    if line.text.contains("\\|") || line.text.contains('`') {
        return None;
    }
    let trimmed_start = line.text.find('|')?;
    let trimmed_end = line.text.rfind('|')?;
    if trimmed_start == trimmed_end
        || !line.text[..trimmed_start].trim().is_empty()
        || !line.text[trimmed_end + 1..].trim().is_empty()
    {
        return None;
    }
    let interior = &line.text[trimmed_start + 1..trimmed_end];
    let base = line.start + trimmed_start + 1;
    let mut cells = Vec::new();
    let mut offset = 0;
    for raw in interior.split('|') {
        let leading = raw.len() - raw.trim_start().len();
        let trailing = raw.len() - raw.trim_end().len();
        let start = base + offset + leading;
        let end = base + offset + raw.len().saturating_sub(trailing);
        cells.push(TableCell {
            start,
            end,
            material: &line.text[start - line.start..end - line.start],
        });
        offset += raw.len() + 1;
    }
    Some(cells)
}

fn is_separator_cell(material: &str) -> bool {
    let material = material.trim();
    let core = material.trim_start_matches(':').trim_end_matches(':');
    core.len() >= 3 && core.bytes().all(|byte| byte == b'-')
}

fn pure_markdown_structure(material: &str) -> bool {
    material
        .trim()
        .chars()
        .all(|character| character.is_ascii_punctuation() || character.is_whitespace())
}

pub(super) async fn build_v2_inputs(
    dossier: &CompletionReviewDossier,
    plan: &ClassificationPlan,
) -> Result<Vec<UserInput>, ReviewFailureCategory> {
    let request = render_v2_request(dossier, plan)?;
    if approx_token_count(&request) > MAX_RENDERED_REQUEST_TOKENS {
        return Err(ReviewFailureCategory::OversizedRequest);
    }
    let mut inputs = vec![UserInput::Text {
        text: request,
        text_elements: Vec::new(),
    }];
    let unresolved_image_ids = plan
        .ranges
        .iter()
        .filter(|range| range.source.source_kind == "image")
        .map(|range| range.source.source_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut image_count = 0usize;
    let mut image_bytes = 0usize;
    for source in dossier
        .sources
        .iter()
        .filter(|source| unresolved_image_ids.contains(source.source_id.as_str()))
    {
        image_count = image_count
            .checked_add(1)
            .ok_or(ReviewFailureCategory::OversizedRequest)?;
        if image_count > MAX_RETAINED_USER_IMAGES {
            return Err(ReviewFailureCategory::OversizedRequest);
        }
        let bytes = if let Some(path) = local_image_path_from_material(&source.exact_material) {
            usize::try_from(
                tokio::fs::metadata(Path::new(path))
                    .await
                    .map_err(|_| ReviewFailureCategory::SourceDrift)?
                    .len(),
            )
            .map_err(|_| ReviewFailureCategory::OversizedRequest)?
        } else {
            source.exact_material.len()
        };
        image_bytes = image_bytes
            .checked_add(bytes)
            .ok_or(ReviewFailureCategory::OversizedRequest)?;
        if image_bytes > MAX_RETAINED_USER_IMAGE_BYTES {
            return Err(ReviewFailureCategory::OversizedRequest);
        }
        if let Some(path) = local_image_path_from_material(&source.exact_material) {
            inputs.push(UserInput::LocalImage {
                path: path.into(),
                detail: None,
            });
        } else {
            inputs.push(UserInput::Image {
                image_url: source.exact_material.clone(),
                detail: None,
            });
        }
    }
    Ok(inputs)
}

fn render_v2_request(
    dossier: &CompletionReviewDossier,
    plan: &ClassificationPlan,
) -> Result<String, ReviewFailureCategory> {
    let input = serde_json::to_string_pretty(&json!({
        "root_task_id": dossier.root_task_id,
        "completion_epoch": dossier.completion_epoch,
        "manifest_revision": dossier.manifest_revision,
        "user_source_ledger_hash": dossier.user_source_ledger_hash,
        "unresolved_ranges": plan.ranges,
        "immutable_context": plan.contexts,
        "locked_requirements": plan.locked,
    }))
    .map_err(|_| ReviewFailureCategory::InputUnavailable)?;
    Ok(format!(
        "{SOURCE_CLASSIFICATION_V2_MARKER}\n\nClassify every declared unresolved range exactly once. Immutable context and locked entries are read-only and non-returnable. Mint requirements only inside the referenced unresolved range. Return a source_disposition exactly once for each model-owned pending source that ends with no merged requirements, and never for a requirement-bearing or host-owned source. Return sparse locked_requirement_overlays only for status or superseded_by changes; copy every identity field exactly. UnavailableOrTruncated is host-owned and is not a permitted range result.\n\n<classification_v2_input>\n{input}\n</classification_v2_input>"
    ))
}

pub(super) fn v2_schema() -> Value {
    let identity = json!({
        "source_id": { "type": "string" },
        "source_hash": { "type": "string" },
        "source_kind": { "type": "string", "enum": ["text", "image", "attachment"] },
        "source_ordinal": { "type": "integer", "minimum": 0 },
        "content_ordinal": { "type": "integer", "minimum": 0 }
    });
    let mut range_properties = identity.as_object().cloned().unwrap_or_default();
    range_properties.insert("range_id".to_string(), json!({"type":"string"}));
    range_properties.insert("source_span".to_string(), wire_span_schema());
    range_properties.insert("result".to_string(), json!({"type":"string","enum":["requirement_bearing","non_requirement","superseded_context"]}));
    range_properties.insert(
        "requirements".to_string(),
        json!({"type":"array","items":classification_requirement_schema()}),
    );
    range_properties.insert("reason".to_string(), json!({"type":"string"}));
    let mut disposition_properties = identity.as_object().cloned().unwrap_or_default();
    disposition_properties.insert(
        "disposition".to_string(),
        json!({"type":"string","enum":["non_requirement","superseded_context"]}),
    );
    disposition_properties.insert("reason".to_string(), json!({"type":"string"}));
    let mut locked_properties = identity.as_object().cloned().unwrap_or_default();
    locked_properties.insert("requirement_id".to_string(), json!({"type":"string"}));
    locked_properties.insert("source_span".to_string(), wire_span_schema());
    locked_properties.insert(
        "status".to_string(),
        json!({"type":"string","enum":["active","superseded","withdrawn"]}),
    );
    locked_properties.insert(
        "superseded_by".to_string(),
        json!({"anyOf":[{"type":"null"},locked_identity_schema()]}),
    );
    json!({
        "type":"object",
        "additionalProperties":false,
        "properties":{
            "range_classifications":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":range_properties,"required":["range_id","source_id","source_hash","source_kind","source_ordinal","content_ordinal","source_span","result","requirements","reason"]}},
            "source_dispositions":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":disposition_properties,"required":["source_id","source_hash","source_kind","source_ordinal","content_ordinal","disposition","reason"]}},
            "locked_requirement_overlays":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":locked_properties,"required":["requirement_id","source_id","source_hash","source_kind","source_ordinal","content_ordinal","source_span","status","superseded_by"]}}
        },
        "required":["range_classifications","source_dispositions","locked_requirement_overlays"]
    })
}

fn classification_requirement_schema() -> Value {
    json!({
        "type":"object","additionalProperties":false,
        "properties":{
            "source_span":wire_span_schema(),
            "status":{"type":"string","enum":["active","superseded","withdrawn"]},
            "superseded_by_source_id":{"type":"string"},
            "superseded_by_span":wire_span_schema()
        },
        "required":["source_span","status","superseded_by_source_id","superseded_by_span"]
    })
}

fn locked_identity_schema() -> Value {
    json!({
        "type":"object","additionalProperties":false,
        "properties":{
            "requirement_id":{"type":"string"},"source_id":{"type":"string"},
            "source_hash":{"type":"string"},"source_kind":{"type":"string","enum":["text","image","attachment"]},
            "source_ordinal":{"type":"integer","minimum":0},"content_ordinal":{"type":"integer","minimum":0},
            "source_span":wire_span_schema()
        },
        "required":["requirement_id","source_id","source_hash","source_kind","source_ordinal","content_ordinal","source_span"]
    })
}

pub(super) fn validate_v2(
    dossier: &CompletionReviewDossier,
    plan: &ClassificationPlan,
    output: SourceClassificationOutputV2,
) -> Option<Vec<ClassifiedSource>> {
    let expected_ranges = plan
        .ranges
        .iter()
        .map(|range| (range.range_id.as_str(), range))
        .collect::<BTreeMap<_, _>>();
    let returned_range_ids = output
        .range_classifications
        .iter()
        .map(|range| range.range_id.as_str())
        .collect::<BTreeSet<_>>();
    if returned_range_ids.len() != output.range_classifications.len()
        || returned_range_ids.len() != expected_ranges.len()
        || returned_range_ids != expected_ranges.keys().copied().collect()
    {
        return None;
    }
    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut minted = BTreeMap::<String, Vec<ClassifiedRequirement>>::new();
    let mut minted_refs = BTreeSet::<ClassifiedRequirementRef>::new();
    for result in &output.range_classifications {
        let expected = expected_ranges.get(result.range_id.as_str())?;
        if !range_identity_matches(expected, result) {
            return None;
        }
        let source = sources.get(result.source_id.as_str())?;
        let declared_span = wire_span_to_source_span(source, &result.source_span)?;
        let valid_shape = match result.result {
            V2RangeResultKind::RequirementBearing => {
                !result.requirements.is_empty() && result.reason.is_empty()
            }
            V2RangeResultKind::NonRequirement | V2RangeResultKind::SupersededContext => {
                result.requirements.is_empty() && !result.reason.trim().is_empty()
            }
        };
        if !valid_shape {
            return None;
        }
        for wire_requirement in &result.requirements {
            let source_span = wire_span_to_source_span(source, &wire_requirement.source_span)?;
            if !span_within(&source_span, &declared_span) {
                return None;
            }
            let reference = ClassifiedRequirementRef {
                source_id: source.source_id.clone(),
                source_span: source_span.clone(),
            };
            if !minted_refs.insert(reference) {
                return None;
            }
            minted
                .entry(source.source_id.clone())
                .or_default()
                .push(ClassifiedRequirement {
                    source_span,
                    status: wire_status(wire_requirement.status),
                    superseded_by: wire_superseded_ref(dossier, wire_requirement)?,
                });
        }
    }

    let expected_locked = plan
        .locked
        .iter()
        .map(|identity| (identity.requirement_id.as_str(), identity))
        .collect::<BTreeMap<_, _>>();
    let mut overlays = BTreeMap::<String, &V2LockedOverlay>::new();
    for overlay in &output.locked_requirement_overlays {
        let expected = expected_locked.get(overlay.requirement_id.as_str())?;
        if overlays
            .insert(overlay.requirement_id.clone(), overlay)
            .is_some()
            || !overlay_identity_matches(expected, overlay)
        {
            return None;
        }
        if let Some(target) = &overlay.superseded_by {
            let expected_target = expected_locked.get(target.requirement_id.as_str())?;
            if !locked_wire_matches(expected_target, target)
                || target.requirement_id == overlay.requirement_id
            {
                return None;
            }
        }
        match overlay.status {
            WireRequirementStatus::Active => return None,
            WireRequirementStatus::Superseded if overlay.superseded_by.is_none() => return None,
            WireRequirementStatus::Withdrawn if overlay.superseded_by.is_some() => {
                return None;
            }
            _ => {}
        }
    }

    let dispositions = output
        .source_dispositions
        .iter()
        .map(|disposition| (disposition.source_id.as_str(), disposition))
        .collect::<BTreeMap<_, _>>();
    if dispositions.len() != output.source_dispositions.len()
        || output.source_dispositions.iter().any(|disposition| {
            disposition.reason.trim().is_empty()
                || sources
                    .get(disposition.source_id.as_str())
                    .is_none_or(|source| !source_identity_matches(source, disposition))
        })
    {
        return None;
    }

    let mut classifications = Vec::new();
    for source in &dossier.sources {
        let mapping = dossier.source_mappings.get(&source.source_id)?;
        if !matches!(mapping, SourceMapping::PendingClassification) {
            let mut terminal = classification_for_terminal(dossier, source, mapping)?;
            apply_overlays(dossier, plan, &overlays, source, &mut terminal.requirements)?;
            if dispositions.contains_key(source.source_id.as_str()) {
                return None;
            }
            classifications.push(terminal);
            continue;
        }
        if let Some(local) = plan.locally_complete.get(&source.source_id) {
            let mut local = local.clone();
            apply_overlays(dossier, plan, &overlays, source, &mut local.requirements)?;
            if dispositions.contains_key(source.source_id.as_str()) {
                return None;
            }
            classifications.push(local);
            continue;
        }
        let mut requirements = plan
            .locally_locked
            .get(&source.source_id)
            .cloned()
            .unwrap_or_default();
        apply_overlays(dossier, plan, &overlays, source, &mut requirements)?;
        requirements.extend(minted.remove(&source.source_id).unwrap_or_default());
        if requirements.is_empty() {
            let disposition = dispositions.get(source.source_id.as_str())?;
            classifications.push(ClassifiedSource {
                source_id: source.source_id.clone(),
                kind: match disposition.disposition {
                    V2SourceDispositionKind::NonRequirement => ClassifiedSourceKind::NonRequirement,
                    V2SourceDispositionKind::SupersededContext => {
                        ClassifiedSourceKind::SupersededContext
                    }
                },
                requirements,
                reason: Some(disposition.reason.clone()),
            });
        } else {
            if dispositions.contains_key(source.source_id.as_str()) {
                return None;
            }
            classifications.push(ClassifiedSource {
                source_id: source.source_id.clone(),
                kind: ClassifiedSourceKind::RequirementBearing,
                requirements,
                reason: None,
            });
        }
    }
    (dispositions.len()
        == classifications
            .iter()
            .filter(|classification| {
                matches!(
                    classification.kind,
                    ClassifiedSourceKind::NonRequirement | ClassifiedSourceKind::SupersededContext
                ) && expected_ranges
                    .values()
                    .any(|range| range.source.source_id == classification.source_id)
            })
            .count())
    .then_some(classifications)
}

fn range_identity_matches(expected: &UnresolvedRange, actual: &V2RangeClassification) -> bool {
    expected.range_id == actual.range_id
        && expected.source.source_id == actual.source_id
        && expected.source.source_hash == actual.source_hash
        && expected.source.source_kind == actual.source_kind
        && expected.source.source_ordinal == actual.source_ordinal
        && expected.source.content_ordinal == actual.content_ordinal
        && expected.source_span == actual.source_span
}

fn source_identity_matches(source: &UserSourceRecord, actual: &V2SourceDisposition) -> bool {
    source.source_id == actual.source_id
        && source.content_hash == actual.source_hash
        && source_kind_name(source.source_kind) == actual.source_kind
        && source.source_ordinal == actual.source_ordinal
        && source.content_ordinal == actual.content_ordinal
}

fn overlay_identity_matches(
    expected: &LockedRequirementIdentity,
    actual: &V2LockedOverlay,
) -> bool {
    expected.requirement_id == actual.requirement_id
        && expected.source.source_id == actual.source_id
        && expected.source.source_hash == actual.source_hash
        && expected.source.source_kind == actual.source_kind
        && expected.source.source_ordinal == actual.source_ordinal
        && expected.source.content_ordinal == actual.content_ordinal
        && expected.source_span == actual.source_span
}

fn locked_wire_matches(
    expected: &LockedRequirementIdentity,
    actual: &LockedRequirementIdentityWire,
) -> bool {
    expected.requirement_id == actual.requirement_id
        && expected.source.source_id == actual.source_id
        && expected.source.source_hash == actual.source_hash
        && expected.source.source_kind == actual.source_kind
        && expected.source.source_ordinal == actual.source_ordinal
        && expected.source.content_ordinal == actual.content_ordinal
        && expected.source_span == actual.source_span
}

fn span_within(candidate: &SourceSpan, declared: &SourceSpan) -> bool {
    match (candidate, declared) {
        (
            SourceSpan::Text {
                start: candidate_start,
                end: candidate_end,
            },
            SourceSpan::Text {
                start: declared_start,
                end: declared_end,
            },
        ) => declared_start <= candidate_start && candidate_end <= declared_end,
        (SourceSpan::Image { .. }, SourceSpan::Image { .. })
        | (SourceSpan::Attachment { .. }, SourceSpan::Attachment { .. }) => candidate == declared,
        _ => false,
    }
}

fn wire_status(status: WireRequirementStatus) -> RequirementStatus {
    match status {
        WireRequirementStatus::Active => RequirementStatus::Active,
        WireRequirementStatus::Superseded => RequirementStatus::Superseded,
        WireRequirementStatus::Withdrawn => RequirementStatus::Withdrawn,
    }
}

fn wire_superseded_ref(
    dossier: &CompletionReviewDossier,
    requirement: &ClassificationRequirement,
) -> Option<Option<ClassifiedRequirementRef>> {
    match requirement.status {
        WireRequirementStatus::Active | WireRequirementStatus::Withdrawn => {
            (requirement.superseded_by_source_id.is_empty()
                && requirement.superseded_by_span == empty_wire_span())
            .then_some(None)
        }
        WireRequirementStatus::Superseded => {
            let source = dossier
                .sources
                .iter()
                .find(|source| source.source_id == requirement.superseded_by_source_id)?;
            Some(Some(ClassifiedRequirementRef {
                source_id: source.source_id.clone(),
                source_span: wire_span_to_source_span(source, &requirement.superseded_by_span)?,
            }))
        }
    }
}

fn empty_wire_span() -> WireSpan {
    WireSpan {
        kind: "text".to_string(),
        start: 0,
        end: 0,
        reference: String::new(),
        subreference: String::new(),
    }
}

fn apply_overlays(
    dossier: &CompletionReviewDossier,
    plan: &ClassificationPlan,
    overlays: &BTreeMap<String, &V2LockedOverlay>,
    source: &UserSourceRecord,
    requirements: &mut [ClassifiedRequirement],
) -> Option<()> {
    for requirement in requirements {
        let id = deterministic_requirement_id(source, &requirement.source_span);
        let Some(overlay) = overlays.get(&id) else {
            continue;
        };
        let previous = dossier
            .requirements
            .iter()
            .find(|previous| previous.requirement_id == id);
        if previous.is_some_and(|previous| previous.status != RequirementStatus::Active) {
            return None;
        }
        requirement.status = wire_status(overlay.status);
        requirement.superseded_by = overlay.superseded_by.as_ref().and_then(|target| {
            plan.locked
                .iter()
                .find(|locked| locked.requirement_id == target.requirement_id)
                .and_then(|locked| {
                    let target_source = dossier
                        .sources
                        .iter()
                        .find(|source| source.source_id == locked.source.source_id)?;
                    Some(ClassifiedRequirementRef {
                        source_id: target_source.source_id.clone(),
                        source_span: wire_span_to_source_span(target_source, &locked.source_span)?,
                    })
                })
        });
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_evidence::CurrentRepairSnapshot;
    use codex_protocol::protocol::TaskCompletionGate;

    fn source(material: &str) -> UserSourceRecord {
        UserSourceRecord {
            source_id: "source-1".to_string(),
            message_id: "message-1".to_string(),
            content_hash: "hash".to_string(),
            source_kind: UserSourceKind::Text,
            source_ordinal: 1,
            content_ordinal: 1,
            exact_material: material.to_string(),
            availability: UserSourceAvailability::Available,
            completion_epoch: 1,
            introduced_manifest_revision: 1,
        }
    }

    fn dossier_with(
        sources: Vec<UserSourceRecord>,
        source_mappings: BTreeMap<String, SourceMapping>,
        requirements: Vec<RequirementRecord>,
    ) -> CompletionReviewDossier {
        CompletionReviewDossier {
            document_revision: 7,
            root_task_id: "root-task".to_string(),
            completion_epoch: 1,
            manifest_revision: 1,
            sources,
            source_mappings,
            source_classification_cache: BTreeMap::new(),
            source_classification_current: false,
            relationship_resolution_current: false,
            mappings_classified: false,
            source_capture_failed: false,
            requirements,
            user_source_ledger_hash: "source-ledger-hash".to_string(),
            requirement_manifest_hash: "manifest-hash".to_string(),
            implementation_identity_hash: "implementation-hash".to_string(),
            dossier_snapshot_id: "dossier-hash".to_string(),
            host_mutation_revision: 3,
            has_task_attributed_mutations: true,
            evidence_gate: TaskCompletionGate {
                status: TaskCompletionStatus::Passed,
                reasons: Vec::new(),
                evidence_path: None,
            },
            locally_obtainable_proof_routes: Vec::new(),
            reviewer_visible_evidence: json!({}),
            review_lens_selection_facts: ReviewLensSelectionFacts::default(),
            authoritative_input_errors: Vec::new(),
            typed_quiescent: true,
            default_children_quiescent: true,
            candidate_completion: Some("done".to_string()),
            correction_consumed: false,
            cycle_phase: Some(CompletionReviewCyclePhase::InitialReviewPending),
            active_cycle_id: Some("cycle-1".to_string()),
            cycle_parent_review_id: None,
            cycle_superseded_review_id: None,
            accepted_review_id: None,
            initial_review_id: None,
            initial_repair_instruction_hash: None,
            original_findings: Vec::new(),
            manifest_gap_reconstructed: false,
            current_repair_snapshot: CurrentRepairSnapshot {
                repository_root: String::new(),
                path_states: Vec::new(),
                command_receipts: Vec::new(),
                plan_structure_hash: String::new(),
                declared_path_scopes: Vec::new(),
                implementation_surfaces: Vec::new(),
                default_child_mutation_identities: Vec::new(),
                typed_mutation_identities: Vec::new(),
                external_evidence_ids: Vec::new(),
                containment_errors: Vec::new(),
            },
            initial_repair_baseline: None,
            initial_repair_baseline_hash: None,
            rereview_input: None,
        }
    }

    fn pending_dossier(sources: Vec<UserSourceRecord>) -> CompletionReviewDossier {
        let mappings = sources
            .iter()
            .map(|source| {
                (
                    source.source_id.clone(),
                    SourceMapping::PendingClassification,
                )
            })
            .collect();
        dossier_with(sources, mappings, Vec::new())
    }

    fn host_mixed_dossier(unresolved_material: &str) -> CompletionReviewDossier {
        let mut unavailable = source("host-owned unavailable bytes");
        unavailable.source_id = "source-unavailable".to_string();
        unavailable.content_hash = "unavailable-hash".to_string();
        unavailable.availability = UserSourceAvailability::Unavailable;
        let mut unresolved = source(unresolved_material);
        unresolved.source_id = "source-unresolved".to_string();
        unresolved.content_hash = "unresolved-hash".to_string();
        unresolved.source_ordinal = 2;
        pending_dossier(vec![unavailable, unresolved])
    }

    fn v2_plan(dossier: &CompletionReviewDossier) -> ClassificationPlan {
        let Some(ClassificationRoute::V2(plan)) = plan_classification(dossier) else {
            panic!("fixture must require V2 classification");
        };
        plan
    }

    fn non_requirement_range(range: &UnresolvedRange) -> V2RangeClassification {
        V2RangeClassification {
            range_id: range.range_id.clone(),
            source_id: range.source.source_id.clone(),
            source_hash: range.source.source_hash.clone(),
            source_kind: range.source.source_kind.clone(),
            source_ordinal: range.source.source_ordinal,
            content_ordinal: range.source.content_ordinal,
            source_span: range.source_span.clone(),
            result: V2RangeResultKind::NonRequirement,
            requirements: Vec::new(),
            reason: "model-owned prose is not a requirement".to_string(),
        }
    }

    fn non_requirement_disposition(source: &UserSourceRecord) -> V2SourceDisposition {
        V2SourceDisposition {
            source_id: source.source_id.clone(),
            source_hash: source.content_hash.clone(),
            source_kind: source_kind_name(source.source_kind).to_string(),
            source_ordinal: source.source_ordinal,
            content_ordinal: source.content_ordinal,
            disposition: V2SourceDispositionKind::NonRequirement,
            reason: "the complete source is non-requirement context".to_string(),
        }
    }

    #[test]
    fn partitions_single_line_requirements_with_gap_free_utf8_coverage() {
        let source = source("# Requirements\n- Must preserve café spans.\n\n");
        let partition = partition_text(&source).expect("partition");
        validate_partition(&source.exact_material, &partition.spans).expect("coverage");
        assert!(
            partition
                .spans
                .iter()
                .any(|span| span.kind == PartitionKind::LockedRequirement)
        );
        assert!(
            !partition
                .spans
                .iter()
                .any(|span| span.kind == PartitionKind::Unresolved)
        );
    }

    #[test]
    fn exclusions_task_lists_multiline_nested_code_quotes_and_vetoes_are_unresolved() {
        for material in [
            "# Notes\n- Must be fast.\n",
            "# Background\n- [x] historical work\n",
            "# Requirements\n- Must work\n  across lines\n",
            "# Requirements\n- Must work\nacross lines\n",
            "# Requirements\n- Must work\n\n  across lines\n",
            "# Requirements\n- Must work\n  - child\n",
            "```\n- Must not panic\n```\n",
            "> - Must not panic\n",
            "# Requirements\n- Replace the old path.\n",
            "# Notes\n- `must not panic`\n",
        ] {
            let source = source(material);
            let partition = partition_text(&source).expect("partition");
            assert!(
                partition
                    .spans
                    .iter()
                    .any(|span| span.kind == PartitionKind::Unresolved)
            );
            assert!(
                !partition
                    .spans
                    .iter()
                    .any(|span| span.kind == PartitionKind::LockedRequirement)
            );
        }
    }

    #[test]
    fn accepts_only_narrow_unscoped_requirement_tables() {
        let accepted = source("| Requirement |\n| --- |\n| Must be atomic |\n");
        let accepted_partition = partition_text(&accepted).expect("partition");
        validate_partition(&accepted.exact_material, &accepted_partition.spans).expect("coverage");
        assert!(
            accepted_partition
                .spans
                .iter()
                .any(|span| span.kind == PartitionKind::LockedRequirement)
        );
        for material in [
            "| Platform | Requirement |\n| --- | --- |\n| Windows | Must be atomic |\n",
            "| Requirement |\n| --- |\n| `a | b` |\n",
            "| Requirement | Note |\n| --- | --- |\n| Must be atomic | why |\n",
            "| Requirement |\n| --- |\n| Must preserve \\| literally |\n",
            "| Requirement | Note |\n| --- | --- |\n| Must be atomic |\n",
        ] {
            let partition = partition_text(&source(material)).expect("partition");
            assert_eq!(
                partition.spans,
                vec![PartitionSpan {
                    start: 0,
                    end: material.len(),
                    kind: PartitionKind::Unresolved,
                }]
            );
        }
    }

    #[test]
    fn task_list_marker_never_supplies_directive_authority() {
        let source = source("# Tasks\n- [ ] ordinary progress item\n- [ ] Must stay atomic\n");
        let partition = partition_text(&source).expect("partition");
        validate_partition(&source.exact_material, &partition.spans).expect("coverage");
        assert_eq!(
            partition
                .spans
                .iter()
                .filter(|span| span.kind == PartitionKind::LockedRequirement)
                .count(),
            1
        );
        assert!(
            partition
                .spans
                .iter()
                .any(|span| span.kind == PartitionKind::Unresolved)
        );
    }

    #[test]
    fn every_initial_supersession_construction_vetoes_local_extraction() {
        for term in [
            "instead",
            "replace",
            "no longer",
            "previous",
            "withdraw",
            "supersede",
        ] {
            let material = format!("# Requirements\n- Must use {term} mode.\n");
            let partition = partition_text(&source(&material)).expect("partition");
            assert!(
                partition
                    .spans
                    .iter()
                    .any(|span| span.kind == PartitionKind::Unresolved)
            );
            assert!(
                !partition
                    .spans
                    .iter()
                    .any(|span| span.kind == PartitionKind::LockedRequirement)
            );
        }
    }

    #[test]
    fn unresolved_range_retains_its_containing_heading_as_immutable_context() {
        let source = source("# Acceptance Criteria\nThis may need interpretation.\n");
        let partition = partition_text(&source).expect("partition");
        let range = partition
            .spans
            .iter()
            .find(|span| span.kind == PartitionKind::Unresolved)
            .expect("unresolved range");
        assert_eq!(
            partition
                .heading_for_unresolved
                .get(&(range.start, range.end))
                .map(String::as_str),
            Some("Acceptance Criteria")
        );
        assert_eq!(
            &source.exact_material[range.start..range.end],
            "This may need interpretation.\n"
        );
    }

    #[test]
    fn partition_validation_rejects_gaps_overlaps_bounds_and_utf8_splits() {
        let text = "éx";
        for spans in [
            vec![PartitionSpan {
                start: 0,
                end: 2,
                kind: PartitionKind::Structural,
            }],
            vec![
                PartitionSpan {
                    start: 0,
                    end: 2,
                    kind: PartitionKind::Structural,
                },
                PartitionSpan {
                    start: 1,
                    end: 3,
                    kind: PartitionKind::Unresolved,
                },
            ],
            vec![PartitionSpan {
                start: 0,
                end: 4,
                kind: PartitionKind::Structural,
            }],
            vec![
                PartitionSpan {
                    start: 0,
                    end: 1,
                    kind: PartitionKind::Structural,
                },
                PartitionSpan {
                    start: 1,
                    end: 3,
                    kind: PartitionKind::Unresolved,
                },
            ],
        ] {
            assert!(validate_partition(text, &spans).is_none());
        }
    }

    #[test]
    fn v2_schema_excludes_host_owned_unavailable_classification() {
        let schema = v2_schema();
        let rendered = serde_json::to_string(&schema).expect("schema JSON");
        assert!(rendered.contains("requirement_bearing"));
        assert!(rendered.contains("non_requirement"));
        assert!(rendered.contains("superseded_context"));
        assert!(!rendered.contains("unavailable_or_truncated"));
    }

    #[test]
    fn routes_structural_only_ambiguity_through_v1_and_authoritative_mixes_through_v2() {
        let ambiguous = source("# Context\nThis sentence needs interpretation.\n");
        let dossier = pending_dossier(vec![ambiguous]);
        assert!(matches!(
            plan_classification(&dossier),
            Some(ClassificationRoute::V1)
        ));

        let structured = source("# Requirements\n- Must remain atomic.\n");
        let dossier = pending_dossier(vec![structured]);
        let Some(ClassificationRoute::LocalOnly(classifications)) = plan_classification(&dossier)
        else {
            panic!("fully structured input should not call the classifier");
        };
        assert_eq!(classifications.len(), 1);
        assert_eq!(
            classifications[0].kind,
            ClassifiedSourceKind::RequirementBearing
        );

        let mixed = source(
            "# Requirements\n- Must remain atomic.\n\n# Context\nThis sentence needs interpretation.\n",
        );
        let dossier = pending_dossier(vec![mixed]);
        let route = plan_classification(&dossier);
        assert!(
            matches!(route, Some(ClassificationRoute::V2(_))),
            "mixed authoritative input route: {route:?}"
        );

        let mut unavailable = source("bytes must not be sent");
        unavailable.availability = UserSourceAvailability::Unavailable;
        let dossier = pending_dossier(vec![unavailable]);
        let Some(ClassificationRoute::LocalOnly(classifications)) = plan_classification(&dossier)
        else {
            panic!("host-owned availability should not call the classifier");
        };
        assert_eq!(
            classifications[0].kind,
            ClassifiedSourceKind::UnavailableOrTruncated
        );
    }

    #[test]
    fn v2_preserves_terminal_source_mappings_and_rejects_dispositions_for_them() {
        let mut terminal = source("Previously classified context.\n");
        terminal.source_id = "source-terminal".to_string();
        terminal.content_hash = "terminal-hash".to_string();
        let mut unresolved = source("Ambiguous new prose.\n");
        unresolved.source_id = "source-unresolved".to_string();
        unresolved.content_hash = "unresolved-hash".to_string();
        unresolved.source_ordinal = 2;
        let dossier = dossier_with(
            vec![terminal.clone(), unresolved.clone()],
            BTreeMap::from([
                (
                    terminal.source_id.clone(),
                    SourceMapping::NonRequirement {
                        reason: "authoritative prior reason".to_string(),
                    },
                ),
                (
                    unresolved.source_id.clone(),
                    SourceMapping::PendingClassification,
                ),
            ]),
            Vec::new(),
        );
        let plan = v2_plan(&dossier);
        let range_result = non_requirement_range(&plan.ranges[0]);
        let unresolved_disposition = non_requirement_disposition(&unresolved);
        let classifications = validate_v2(
            &dossier,
            &plan,
            SourceClassificationOutputV2 {
                range_classifications: vec![range_result.clone()],
                source_dispositions: vec![unresolved_disposition.clone()],
                locked_requirement_overlays: Vec::new(),
            },
        )
        .expect("terminal mapping remains authoritative");
        let preserved = classifications
            .iter()
            .find(|classification| classification.source_id == terminal.source_id)
            .expect("terminal classification");
        assert_eq!(preserved.kind, ClassifiedSourceKind::NonRequirement);
        assert_eq!(
            preserved.reason.as_deref(),
            Some("authoritative prior reason")
        );

        assert!(
            validate_v2(
                &dossier,
                &plan,
                SourceClassificationOutputV2 {
                    range_classifications: vec![range_result],
                    source_dispositions: vec![
                        unresolved_disposition,
                        non_requirement_disposition(&terminal),
                    ],
                    locked_requirement_overlays: Vec::new(),
                },
            )
            .is_none()
        );
    }

    #[test]
    fn v2_locked_overlays_are_sparse_exact_and_cannot_self_supersede() {
        let mut structured = source(
            "Ambiguous prose requiring classification.\n# Requirements\n- Must remain atomic.\n",
        );
        structured.source_id = "source-mixed".to_string();
        structured.content_hash = "mixed-hash".to_string();
        let dossier = pending_dossier(vec![structured]);
        let plan = v2_plan(&dossier);
        assert_eq!(plan.locked.len(), 1);
        let locked = &plan.locked[0];
        let base_overlay = V2LockedOverlay {
            requirement_id: locked.requirement_id.clone(),
            source_id: locked.source.source_id.clone(),
            source_hash: locked.source.source_hash.clone(),
            source_kind: locked.source.source_kind.clone(),
            source_ordinal: locked.source.source_ordinal,
            content_ordinal: locked.source.content_ordinal,
            source_span: locked.source_span.clone(),
            status: WireRequirementStatus::Withdrawn,
            superseded_by: None,
        };
        let range_result = non_requirement_range(&plan.ranges[0]);
        let classifications = validate_v2(
            &dossier,
            &plan,
            SourceClassificationOutputV2 {
                range_classifications: vec![range_result.clone()],
                source_dispositions: Vec::new(),
                locked_requirement_overlays: vec![base_overlay.clone()],
            },
        )
        .expect("exact withdrawn overlay");
        assert_eq!(
            classifications[0].requirements[0].status,
            RequirementStatus::Withdrawn
        );

        let mut self_superseding = base_overlay;
        self_superseding.status = WireRequirementStatus::Superseded;
        self_superseding.superseded_by = Some(LockedRequirementIdentityWire {
            requirement_id: locked.requirement_id.clone(),
            source_id: locked.source.source_id.clone(),
            source_hash: locked.source.source_hash.clone(),
            source_kind: locked.source.source_kind.clone(),
            source_ordinal: locked.source.source_ordinal,
            content_ordinal: locked.source.content_ordinal,
            source_span: locked.source_span.clone(),
        });
        assert!(
            validate_v2(
                &dossier,
                &plan,
                SourceClassificationOutputV2 {
                    range_classifications: vec![range_result],
                    source_dispositions: Vec::new(),
                    locked_requirement_overlays: vec![self_superseding],
                },
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn v2_text_uses_bounded_image_references_and_raw_bytes_only_as_attachments() {
        let raw_image = "data:image/png;base64,super-secret-image-bytes";
        let mut unavailable = source("not available");
        unavailable.source_id = "source-unavailable".to_string();
        unavailable.content_hash = "unavailable-hash".to_string();
        unavailable.availability = UserSourceAvailability::Truncated;
        let mut image = source(raw_image);
        image.source_id = "source-image".to_string();
        image.content_hash = "image-hash".to_string();
        image.source_kind = UserSourceKind::Image;
        image.source_ordinal = 2;
        let dossier = pending_dossier(vec![unavailable, image]);
        let Some(ClassificationRoute::V2(plan)) = plan_classification(&dossier) else {
            panic!("host-owned availability plus an unresolved image requires V2");
        };

        let inputs = build_v2_inputs(&dossier, &plan)
            .await
            .expect("bounded V2 request");
        let UserInput::Text { text, .. } = &inputs[0] else {
            panic!("first V2 input must be text");
        };
        assert!(!text.contains(raw_image));
        assert!(text.contains("kd4-source:source-image#content-hash=image-hash"));
        assert!(matches!(
            inputs.get(1),
            Some(UserInput::Image { image_url, .. }) if image_url == raw_image
        ));
        assert_eq!(inputs.len(), 2);
    }

    #[tokio::test]
    async fn v2_enforces_the_existing_image_count_limit_without_dropping_sources() {
        let mut sources = Vec::new();
        let mut unavailable = source("not available");
        unavailable.source_id = "source-unavailable".to_string();
        unavailable.availability = UserSourceAvailability::Unavailable;
        sources.push(unavailable);
        for ordinal in 1..=MAX_RETAINED_USER_IMAGES {
            let mut image = source(&format!("data:image/png;base64,{ordinal}"));
            image.source_id = format!("source-image-{ordinal}");
            image.content_hash = format!("image-hash-{ordinal}");
            image.source_kind = UserSourceKind::Image;
            image.source_ordinal = ordinal as u64 + 1;
            sources.push(image);
        }
        let dossier = pending_dossier(sources.clone());
        let plan = v2_plan(&dossier);
        assert!(build_v2_inputs(&dossier, &plan).await.is_ok());

        let ordinal = MAX_RETAINED_USER_IMAGES + 1;
        let mut image = source(&format!("data:image/png;base64,{ordinal}"));
        image.source_id = format!("source-image-{ordinal}");
        image.content_hash = format!("image-hash-{ordinal}");
        image.source_kind = UserSourceKind::Image;
        image.source_ordinal = ordinal as u64 + 1;
        sources.push(image);
        let dossier = pending_dossier(sources);
        let plan = v2_plan(&dossier);
        assert!(matches!(
            build_v2_inputs(&dossier, &plan).await,
            Err(ReviewFailureCategory::OversizedRequest)
        ));
    }

    #[test]
    fn v2_requires_exact_range_accountability_and_explicit_source_dispositions() {
        let dossier = host_mixed_dossier("Ambiguous prose requiring classification.\n");
        let plan = v2_plan(&dossier);
        assert_eq!(plan.ranges.len(), 1);
        let range_result = non_requirement_range(&plan.ranges[0]);
        let unresolved_source = dossier
            .sources
            .iter()
            .find(|source| source.source_id == "source-unresolved")
            .expect("unresolved source");
        let disposition = non_requirement_disposition(unresolved_source);

        assert!(
            validate_v2(
                &dossier,
                &plan,
                SourceClassificationOutputV2 {
                    range_classifications: Vec::new(),
                    source_dispositions: vec![disposition.clone()],
                    locked_requirement_overlays: Vec::new(),
                },
            )
            .is_none()
        );
        assert!(
            validate_v2(
                &dossier,
                &plan,
                SourceClassificationOutputV2 {
                    range_classifications: vec![range_result.clone(), range_result.clone()],
                    source_dispositions: vec![disposition.clone()],
                    locked_requirement_overlays: Vec::new(),
                },
            )
            .is_none()
        );
        assert!(
            validate_v2(
                &dossier,
                &plan,
                SourceClassificationOutputV2 {
                    range_classifications: vec![range_result.clone()],
                    source_dispositions: Vec::new(),
                    locked_requirement_overlays: Vec::new(),
                },
            )
            .is_none()
        );
        let classifications = validate_v2(
            &dossier,
            &plan,
            SourceClassificationOutputV2 {
                range_classifications: vec![range_result],
                source_dispositions: vec![disposition],
                locked_requirement_overlays: Vec::new(),
            },
        )
        .expect("complete range and source accountability");
        assert_eq!(classifications.len(), 2);
        assert_eq!(
            classifications
                .iter()
                .find(|classification| classification.source_id == "source-unavailable")
                .expect("host classification")
                .kind,
            ClassifiedSourceKind::UnavailableOrTruncated
        );
    }

    #[test]
    fn v2_confines_minted_requirements_to_the_declared_range() {
        let dossier = host_mixed_dossier("# Context\nAmbiguous prose requiring classification.\n");
        let plan = v2_plan(&dossier);
        let mut range_result = non_requirement_range(&plan.ranges[0]);
        range_result.result = V2RangeResultKind::RequirementBearing;
        range_result.reason.clear();
        range_result.requirements.push(ClassificationRequirement {
            source_span: WireSpan {
                kind: "text".to_string(),
                start: 0,
                end: 1,
                reference: String::new(),
                subreference: String::new(),
            },
            status: WireRequirementStatus::Active,
            superseded_by_source_id: String::new(),
            superseded_by_span: empty_wire_span(),
        });
        assert!(
            validate_v2(
                &dossier,
                &plan,
                SourceClassificationOutputV2 {
                    range_classifications: vec![range_result],
                    source_dispositions: Vec::new(),
                    locked_requirement_overlays: Vec::new(),
                },
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn v2_enforces_the_rendered_request_token_boundary() {
        let request_tokens = |material_len: usize| {
            let dossier = host_mixed_dossier(&"x".repeat(material_len));
            let plan = v2_plan(&dossier);
            approx_token_count(
                &render_v2_request(&dossier, &plan)
                    .expect("test V2 classification request should serialize"),
            )
        };
        let mut fits = 1usize;
        let mut exceeds = MAX_RENDERED_REQUEST_TOKENS * 8;
        assert!(request_tokens(fits) <= MAX_RENDERED_REQUEST_TOKENS);
        assert!(request_tokens(exceeds) > MAX_RENDERED_REQUEST_TOKENS);
        while fits + 1 < exceeds {
            let candidate = fits + (exceeds - fits) / 2;
            if request_tokens(candidate) <= MAX_RENDERED_REQUEST_TOKENS {
                fits = candidate;
            } else {
                exceeds = candidate;
            }
        }
        assert!(request_tokens(fits) <= MAX_RENDERED_REQUEST_TOKENS);
        assert!(request_tokens(exceeds) > MAX_RENDERED_REQUEST_TOKENS);
        let at_limit = host_mixed_dossier(&"x".repeat(fits));
        let at_limit_plan = v2_plan(&at_limit);
        assert!(build_v2_inputs(&at_limit, &at_limit_plan).await.is_ok());
        let above_limit = host_mixed_dossier(&"x".repeat(exceeds));
        let above_limit_plan = v2_plan(&above_limit);
        assert!(matches!(
            build_v2_inputs(&above_limit, &above_limit_plan).await,
            Err(ReviewFailureCategory::OversizedRequest)
        ));
    }

    #[tokio::test]
    async fn v2_applies_the_exact_aggregate_image_byte_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first_size = MAX_RETAINED_USER_IMAGE_BYTES / 2;
        let second_size = MAX_RETAINED_USER_IMAGE_BYTES - first_size;
        let mut sources = Vec::new();
        let mut unavailable = source("host-owned unavailable bytes");
        unavailable.source_id = "source-unavailable".to_string();
        unavailable.availability = UserSourceAvailability::Unavailable;
        sources.push(unavailable);
        let mut paths = Vec::new();
        for (ordinal, size) in [first_size, second_size].into_iter().enumerate() {
            let path = temp.path().join(format!("image-{ordinal}.png"));
            let file = tokio::fs::File::create(&path).await.expect("image fixture");
            file.set_len(size as u64)
                .await
                .expect("set logical image size");
            let mut image = source("unused");
            image.source_id = format!("source-image-{ordinal}");
            image.content_hash = format!("image-hash-{ordinal}");
            image.source_kind = UserSourceKind::Image;
            image.source_ordinal = ordinal as u64 + 2;
            image.exact_material = format!(
                "local-image:{}#sha256={}",
                path.to_string_lossy(),
                "a".repeat(64)
            );
            paths.push(path);
            sources.push(image);
        }
        let dossier = pending_dossier(sources);
        let plan = v2_plan(&dossier);
        assert!(build_v2_inputs(&dossier, &plan).await.is_ok());

        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&paths[1])
            .await
            .expect("second image fixture");
        file.set_len(second_size as u64 + 1)
            .await
            .expect("increase logical image size");
        assert!(matches!(
            build_v2_inputs(&dossier, &plan).await,
            Err(ReviewFailureCategory::OversizedRequest)
        ));
    }

    #[test]
    fn attachment_ranges_require_complete_reference_identity() {
        let declared = SourceSpan::Attachment {
            reference: "attachment.pdf".to_string(),
            range: Some("page 2".to_string()),
        };
        assert!(span_within(&declared, &declared));
        assert!(!span_within(
            &SourceSpan::Attachment {
                reference: "attachment.pdf".to_string(),
                range: Some("page 3".to_string()),
            },
            &declared,
        ));
    }
}
