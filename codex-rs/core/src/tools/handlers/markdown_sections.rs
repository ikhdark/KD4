//! Markdown headings over canonical source bytes, not rendered Markdown.
use std::time::Instant;

use codex_tools::CanonicalByteRange;
use codex_tools::ToolProjectionInclusion;
use codex_tools::ToolProjectionSection;
use pulldown_cmark::Event;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;
use serde::Serialize;

#[derive(Serialize)]
struct Heading {
    id: String,
    title: String,
    title_truncated: bool,
    level: u8,
    parent_id: Option<String>,
    start_line: usize,
    end_line: usize,
    heading_range: CanonicalByteRange,
    canonical_range: CanonicalByteRange,
}

pub(super) fn sections(
    path: &str,
    source: &str,
    sha256: &str,
    deadline: Instant,
) -> Option<Vec<ToolProjectionSection>> {
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    if !extension.eq_ignore_ascii_case("md") && !extension.eq_ignore_ascii_case("markdown") {
        return None;
    }
    let mut line_starts = vec![0];
    for (offset, byte) in source.bytes().enumerate() {
        if offset % 4096 == 0 && Instant::now() >= deadline {
            return None;
        }
        if byte == b'\n' {
            line_starts.push(offset + 1);
        }
    }
    let mut headings: Vec<Heading> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut current = None;
    // The CommonMark block parser excludes fenced/indented code and HTML blocks
    // and recognizes both ATX and multiline Setext headings with exact offsets.
    for (event, range) in Parser::new(source).into_offset_iter() {
        if Instant::now() >= deadline {
            return None;
        }
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let level = level as u8;
                let start_line = line_starts.partition_point(|start| *start <= range.start);
                let start = line_starts[start_line - 1];
                while stack.last().is_some_and(|index| headings[*index].level >= level) {
                    let index = stack.pop()?;
                    headings[index].canonical_range.end = start as u64;
                    headings[index].end_line = start_line - 1;
                }
                let index = headings.len();
                headings.push(Heading {
                    id: format!("markdown:{sha256}:{start}"),
                    title: String::new(),
                    title_truncated: false,
                    level,
                    parent_id: stack.last().map(|parent| headings[*parent].id.clone()),
                    start_line,
                    end_line: line_starts.partition_point(|start| *start < source.len()).max(1),
                    heading_range: CanonicalByteRange::new(start as u64, range.end as u64),
                    canonical_range: CanonicalByteRange::new(start as u64, source.len() as u64),
                });
                stack.push(index);
                current = Some(index);
            }
            Event::End(TagEnd::Heading(_)) => current = None,
            event => {
                let Some(index) = current else {
                    continue;
                };
                let text = match &event {
                    Event::Text(text) | Event::Code(text) => text.as_ref(),
                    Event::SoftBreak | Event::HardBreak => " ",
                    _ => continue,
                };
                let heading = &mut headings[index];
                let end = text.floor_char_boundary(512 - heading.title.len());
                heading.title.push_str(&text[..end]);
                heading.title_truncated |= end < text.len();
            }
        }
    }
    let mut sections = Vec::with_capacity(headings.len());
    for heading in &headings {
        if Instant::now() >= deadline {
            return None;
        }
        sections.push(ToolProjectionSection {
            id: heading.id.clone(),
            value: None,
            exact_bytes: heading.canonical_range.end - heading.canonical_range.start,
            inclusion: ToolProjectionInclusion::Included,
            canonical_range: Some(heading.canonical_range),
            children: Vec::new(),
            recovery_chunk_bytes: None,
        });
    }
    sections.extend(super::code_sections::outline_sections(&headings));
    (Instant::now() < deadline).then_some(sections)
}
